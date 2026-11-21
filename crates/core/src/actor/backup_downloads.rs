//! Settings > Backups and the backup media download (see `crate::backups`).
//!
//! A transferred backup's attachments that can come from the backup tier
//! and download whole (the media auto-download choices) are queued in the
//! store, as Desktop queues `backup_import` download jobs, so the download
//! survives a restart and can be paused, resumed and canceled. One file
//! downloads at a time, newest message first. A file that fails is tried
//! again on a later run, up to three times; then (or at once, when it isn't
//! in the backup) its backup thumbnail is fetched instead, as Desktop's
//! download job falls back, and it counts as done. Runs start when the
//! queue fills, each time the message queue drains (a start or a
//! reconnect), on resume, and five minutes after a run left files waiting.

use std::cell::Cell;

use crate::backups::{self, BackupMediaDownloadAction};

use super::*;

/// Tries for one file before falling back to its thumbnail.
const MAX_ATTEMPTS: u32 = 3;
/// After a run left files waiting.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// Desktop's `throttledUpdateBackupMediaDownloadProgress`: 200 ms.
const EMIT_EVERY: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(crate) struct BackupDownloads {
    pub(crate) status: backups::BackupStatusFetcher,
    running: Cell<bool>,
    idle: Cell<bool>,
    /// Bumped by a cancel, so a run in progress stops.
    generation: Cell<u64>,
    /// Failed tries this session, by job.
    attempts: RefCell<HashMap<(i64, usize), u32>>,
    emitted: Cell<Option<Instant>>,
    retry_scheduled: Cell<bool>,
}

enum Outcome {
    Done,
    /// Left queued for a later run.
    Later,
}

/// Whether a failure can't get better by trying again: the file isn't in
/// the backup or the transit tier, or isn't what it should be.
fn permanent(error: &str) -> bool {
    ["isn't in your backup", "404", "doesn't match its hash", "bigger than it should be", "can't decrypt", "receive limit"]
        .iter()
        .any(|marker| error.contains(marker))
}

impl Actor {
    pub(super) async fn backup_command(self: &Rc<Self>, command: Command) {
        match command {
            Command::RefreshBackupStatus { details } => {
                let Some(manager) = self.manager() else {
                    self.emit(Event::BackupStatus(backups::stored_status(&self.db, false).await));
                    return;
                };
                let this = self.clone();
                tokio::task::spawn_local(async move {
                    this.backup_downloads.status.refresh(&manager, &this.db, details, |e| this.emit(e)).await;
                });
            }
            Command::LoadBackupMediaDownload => self.emit_backup_download(true).await,
            Command::BackupMediaDownload(action) => {
                info!(?action, "backup media download");
                let state = &self.backup_downloads;
                if action == BackupMediaDownloadAction::Cancel {
                    state.generation.set(state.generation.get() + 1);
                    state.attempts.borrow_mut().clear();
                }
                backups::apply_action(&self.db, action).await;
                if action == BackupMediaDownloadAction::Resume {
                    // Desktop's `resetBackupAttachmentDownloadJobsRetryAfter`.
                    state.attempts.borrow_mut().clear();
                    self.start_backup_downloads();
                }
                self.emit_backup_download(true).await;
            }
            _ => {}
        }
    }

    /// The account's backup tier changed (Desktop's `saveBackupTier`):
    /// what was fetched for the old one is forgotten and the download's
    /// counts start again.
    pub(super) async fn backup_tier_changed(&self) {
        backups::forget_fetched(&self.db).await;
        backups::reset_download(&self.db).await;
        self.backup_downloads.status.expire();
        self.emit_backup_download(true).await;
    }

    /// The subscriber id from the account record or the backup.
    pub(super) async fn save_backup_subscriber(&self, id: Option<&[u8]>) {
        if backups::save_subscriber_id(&self.db, id).await {
            self.backup_downloads.status.expire();
        }
    }

    pub(super) async fn emit_backup_download(&self, force: bool) {
        let state = &self.backup_downloads;
        if !force && state.emitted.get().is_some_and(|at| at.elapsed() < EMIT_EVERY) {
            return;
        }
        state.emitted.set(Some(Instant::now()));
        self.emit(Event::BackupMediaDownload(backups::download_status(&self.db, state.idle.get()).await));
    }

    /// Splits a transferred backup's downloads: those for the backup media
    /// download are queued (its counts starting again, as Desktop's import
    /// resets them), the rest returned.
    pub(super) async fn queue_backup_media(self: &Rc<Self>, downloads: Vec<(i64, usize)>) -> Vec<(i64, usize)> {
        if crate::backup_media::BackupMedia::root_key(&self.db).await.is_none() {
            return downloads;
        }
        let (mut queued, mut rest) = (Vec::new(), Vec::new());
        for (message_id, index) in downloads {
            let message = self.db.message(message_id).await.ok().flatten();
            let meta = message.as_ref().and_then(|m| m.attachments.get(index));
            match meta.and_then(|a| a.remote.as_ref().map(|r| (a, r))) {
                Some((meta, remote)) if crate::backup_media::media_parts(remote).is_some() && downloads_whole(meta) => {
                    queued.push((message_id, index, crate::backup_media::ciphertext_size(meta.size)));
                }
                _ => rest.push((message_id, index)),
            }
        }
        if !queued.is_empty() {
            info!(files = queued.len(), "queued backup media");
            backups::reset_download(&self.db).await;
            if let Err(error) = self.db.queue_backup_media(&queued).await {
                warn!(%error, "couldn't queue backup media; downloading without progress");
                rest.extend(queued.into_iter().map(|(m, i, _)| (m, i)));
            } else {
                self.start_backup_downloads();
            }
        }
        rest
    }

    /// Starts a run unless one is going.
    pub(super) fn start_backup_downloads(self: &Rc<Self>) {
        if !self.backup_downloads.running.get() {
            tokio::task::spawn_local(self.clone().run_backup_downloads());
        }
    }

    async fn run_backup_downloads(self: Rc<Self>) {
        let state = &self.backup_downloads;
        if state.running.replace(true) {
            return;
        }
        let paused = || async { self.db.setting(backups::SETTING_DOWNLOAD_PAUSED).await.ok().flatten().as_deref() == Some("true") };
        let jobs = self.db.next_backup_media(u32::MAX).await.unwrap_or_default();
        let generation = state.generation.get();
        let mut waiting = 0;
        if !jobs.is_empty() && !paused().await {
            state.idle.set(false);
            self.emit_backup_download(true).await;
            for (message_id, index) in jobs {
                if paused().await || state.generation.get() != generation {
                    break;
                }
                let Some(manager) = self.manager() else { break };
                match self.backup_media_job(&manager, message_id, index).await {
                    Outcome::Done => {
                        if let Err(error) = self.db.finish_backup_media(message_id, index).await {
                            warn!(%error, "couldn't record a finished backup media download");
                        }
                    }
                    Outcome::Later => waiting += 1,
                }
                self.emit_backup_download(false).await;
            }
        }
        state.running.set(false);
        // Files left after a full run wait for the next: shown as idle.
        let left = self.db.backup_media_progress().await.map(|p| p.pending).unwrap_or(0);
        state.idle.set(waiting > 0 && left > 0);
        if waiting > 0 {
            info!(waiting, "backup media downloads to retry later");
            if !state.retry_scheduled.replace(true) {
                let this = self.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(RETRY_AFTER).await;
                    this.backup_downloads.retry_scheduled.set(false);
                    this.start_backup_downloads();
                });
            }
        }
        self.emit_backup_download(true).await;
    }

    async fn backup_media_job(&self, manager: &SignalManager, message_id: i64, index: usize) -> Outcome {
        let Some(message) = self.db.message(message_id).await.ok().flatten() else { return Outcome::Done };
        let Some(meta) = message.attachments.get(index).filter(|a| a.local_path.is_none() && a.remote.is_some()) else {
            return Outcome::Done;
        };
        let error = match self.download_remote(manager, &message.conversation_id, message_id, index, meta).await {
            Ok(()) => return Outcome::Done,
            Err(error) => format!("{error:#}"),
        };
        let tries = {
            let mut attempts = self.backup_downloads.attempts.borrow_mut();
            let tries = attempts.entry((message_id, index)).or_default();
            *tries += 1;
            *tries
        };
        if !permanent(&error) && tries < MAX_ATTEMPTS {
            debug!(%error, tries, "backup media download failed; trying again later");
            return Outcome::Later;
        }
        debug!(%error, tries, "backup media download gave up");
        self.backup_downloads.attempts.borrow_mut().remove(&(message_id, index));
        if let Err(error) = self.fetch_thumbnail(manager, &message, index, meta).await {
            debug!(error = %format!("{error:#}"), "backup thumbnail download failed");
        }
        Outcome::Done
    }
}

#[cfg(test)]
mod tests {
    use super::permanent;

    #[test]
    fn only_lasting_failures_give_up_at_once() {
        assert!(permanent("from your backup: it isn't in your backup; it's only in your backup, which this device can't read"));
        assert!(permanent("from your backup: the download doesn't match its hash"));
        assert!(!permanent("from your backup: error sending request for url; it's only in your backup, which this device can't read"));
    }
}
