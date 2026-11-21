//! Sticker packs, as Signal Desktop handles them (`ts/types/Stickers.preload.ts`,
//! `ts/state/ducks/stickers.preload.ts`): packs named in stickers we
//! receive become known; the sticker manager lists them with the Signal
//! Artist Series and fetches their titles and covers; a pack opens with
//! its stickers; installing and uninstalling tell our other devices (a
//! `StickerPackOperation` sync and the pack's `InstalledStickerPackRecord`),
//! and their records are merged as `mergeInstalledStickerPackRecord` does.
//!
//! Network work is bounded: manifests and covers are fetched one pack at a
//! time, at most [`META_PER_RUN`] packs each time the manager opens; a
//! preview fetches its stickers one by one; packs Storage Service says to
//! install download one at a time.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use crate::stickers::{StickerManagerView, StickerPackPreview, StickerPackSummary, blessed_packs, is_blessed};

use super::*;

/// Packs whose manifest and cover are fetched each time the manager opens.
const META_PER_RUN: usize = 24;
/// Hidden pack records written again at each sync, for changes whose
/// write failed and ones Storage Service doesn't have.
const HIDDEN_WRITES_PER_SYNC: usize = 20;

thread_local! {
    /// A metadata fetch or storage install is running (the actor's thread).
    static FETCHING_META: Cell<bool> = const { Cell::new(false) };
    static INSTALLING: Cell<bool> = const { Cell::new(false) };
}

impl Actor {
    fn sticker_dir(&self, id: &[u8]) -> PathBuf {
        self.config.data_dir.join("stickers").join(hex_of(id))
    }

    /// Writes a sticker's image once, named by the kind it is.
    pub(super) fn save_sticker_image(dir: &Path, id: u32, bytes: &[u8]) -> Option<PathBuf> {
        std::fs::create_dir_all(dir).ok()?;
        let ext = media::ImageKind::sniff(bytes).map_or("webp", |k| k.extension());
        let path = dir.join(format!("{id}.{ext}"));
        if !path.exists() {
            std::fs::write(&path, bytes).ok()?;
        }
        Some(path)
    }

    /// A sticker we received names its pack: known from now on.
    pub(super) async fn sticker_pack_seen(&self, id: &[u8], key: &[u8]) {
        if let Err(error) = self.db.sticker_pack_seen(&hex_of(id), &hex_of(key), now_millis()).await {
            warn!(%error, "failed to remember a sticker pack");
        }
    }

    /// What the manager lists: installed packs in order, the Artist Series
    /// and received packs, from presage's installed packs and our rows.
    pub(super) async fn sticker_manager_view(&self) -> StickerManagerView {
        let installed = self.load_stickers().await;
        let rows = self.db.sticker_pack_rows().await.unwrap_or_default();
        let summary_of_row = |row: &langis_store::StickerPackRow| -> Option<StickerPackSummary> {
            let id = hex::decode(&row.id).ok()?;
            let installed_view = installed.iter().find(|p| p.id == id);
            Some(StickerPackSummary {
                key: hex::decode(&row.key).ok()?,
                title: row.title.clone().or_else(|| installed_view.map(|p| p.title.clone())).unwrap_or_default(),
                author: row.author.clone().or_else(|| installed_view.map(|p| p.author.clone())).unwrap_or_default(),
                cover: row
                    .cover_path
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| p.exists())
                    .or_else(|| installed_view.and_then(|p| p.stickers.first().map(|s| s.path.clone()))),
                sticker_count: if row.sticker_count > 0 { row.sticker_count } else { installed_view.map_or(0, |p| p.stickers.len() as u32) },
                installed: installed_view.is_some(),
                blessed: is_blessed(&id),
                id,
            })
        };
        let installed_summaries = installed
            .iter()
            .map(|pack| match rows.iter().find(|r| r.id == hex_of(&pack.id)).and_then(summary_of_row) {
                Some(summary) => summary,
                None => StickerPackSummary {
                    id: pack.id.clone(),
                    key: pack.key.clone(),
                    title: pack.title.clone(),
                    author: pack.author.clone(),
                    cover: pack.stickers.first().map(|s| s.path.clone()),
                    sticker_count: pack.stickers.len() as u32,
                    installed: true,
                    blessed: is_blessed(&pack.id),
                },
            })
            .collect();
        let blessed = blessed_packs()
            .into_iter()
            .map(|(id, key)| match rows.iter().find(|r| r.id == hex_of(&id)).and_then(summary_of_row) {
                Some(summary) => summary,
                None => StickerPackSummary {
                    installed: installed.iter().any(|p| p.id == id),
                    id,
                    key,
                    title: String::new(),
                    author: String::new(),
                    cover: None,
                    sticker_count: 0,
                    blessed: true,
                },
            })
            .collect();
        // Desktop's "Shared With You": every pack we know that isn't the
        // Artist Series, installed ones included, newest first.
        // Less the ones hidden (Desktop's `getReceivedStickerPacks`).
        let hidden = self.db.hidden_sticker_pack_ids().await.unwrap_or_default();
        let received = rows
            .iter()
            .filter(|r| hex::decode(&r.id).is_ok_and(|id| !is_blessed(&id)) && !hidden.contains(&r.id))
            .filter_map(summary_of_row)
            .collect();
        StickerManagerView { installed: installed_summaries, blessed, received }
    }

    /// The manager opened: what it lists now, then (one pack at a time)
    /// the titles and covers it doesn't have yet, showing each as it comes.
    pub(super) async fn load_sticker_manager(self: &Rc<Self>) {
        self.emit(Event::StickerManager(self.sticker_manager_view().await));
        if FETCHING_META.replace(true) {
            return;
        }
        let this = self.clone();
        tokio::task::spawn_local(async move {
            this.fetch_pack_meta().await;
            FETCHING_META.set(false);
        });
    }

    async fn fetch_pack_meta(self: &Rc<Self>) {
        let Some(mut manager) = self.manager() else { return };
        let now = now_millis();
        // The Artist Series is known from the start, as Desktop's `load`.
        for (id, key) in blessed_packs() {
            let _ = self.db.sticker_pack_seen(&hex_of(&id), &hex_of(&key), now).await;
        }
        let rows = self.db.sticker_pack_rows().await.unwrap_or_default();
        let missing: Vec<_> = rows.into_iter().filter(|r| r.title.is_none()).take(META_PER_RUN).collect();
        for row in missing {
            let (Ok(id), Ok(key)) = (hex::decode(&row.id), hex::decode(&row.key)) else { continue };
            let manifest = match manager.sticker_pack_manifest(&id, &key).await {
                Ok(manifest) => manifest,
                Err(error) => {
                    debug!(%error, "couldn't fetch a sticker pack's manifest");
                    continue;
                }
            };
            let cover_id = manifest.cover.as_ref().map(|c| c.id).or_else(|| manifest.stickers.first().map(|s| s.id));
            let mut cover = None;
            if let Some(cover_id) = cover_id {
                match manager.sticker_image(&id, &key, cover_id).await {
                    Ok(bytes) => cover = Self::save_sticker_image(&self.sticker_dir(&id).join("cover"), cover_id, &bytes),
                    Err(error) => debug!(%error, "couldn't fetch a sticker pack's cover"),
                }
            }
            let cover = cover.map(|p| p.to_string_lossy().into_owned());
            let count = manifest.stickers.len() as u32;
            if let Err(error) = self.db.set_sticker_pack_meta(&row.id, &manifest.title, &manifest.author, cover.as_deref(), count).await {
                warn!(%error, "failed to save a sticker pack's title");
                continue;
            }
            self.emit(Event::StickerManager(self.sticker_manager_view().await));
        }
    }

    /// Opens a pack: an installed one from presage, else its manifest and
    /// stickers fetched (one at a time) into the cache.
    pub(super) async fn preview_sticker_pack(&self, id: Vec<u8>, key: Vec<u8>) {
        let result = self.try_preview_sticker_pack(&id, &key).await.map_err(|error| {
            debug!(error = %format!("{error:#}"), "couldn't open a sticker pack");
            "Error opening sticker pack. Check your internet connection and try again.".to_owned()
        });
        self.emit(Event::StickerPackPreview { id, result });
    }

    async fn try_preview_sticker_pack(&self, id: &[u8], key: &[u8]) -> anyhow::Result<StickerPackPreview> {
        anyhow::ensure!(id.len() == 16 && key.len() == 32, "not a sticker pack");
        if let Some(pack) = self.load_stickers().await.into_iter().find(|p| p.id == id) {
            return Ok(StickerPackPreview {
                pack: StickerPackSummary {
                    id: pack.id.clone(),
                    key: pack.key.clone(),
                    title: pack.title.clone(),
                    author: pack.author.clone(),
                    cover: pack.stickers.first().map(|s| s.path.clone()),
                    sticker_count: pack.stickers.len() as u32,
                    installed: true,
                    blessed: is_blessed(&pack.id),
                },
                stickers: pack.stickers,
            });
        }
        let mut manager = self.manager().context("not connected")?;
        let manifest = manager.sticker_pack_manifest(id, key).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let dir = self.config.data_dir.join("stickers").join("preview").join(hex_of(id));
        let mut stickers = Vec::new();
        for sticker in &manifest.stickers {
            let existing = std::fs::read_dir(&dir).ok().and_then(|entries| {
                entries.flatten().map(|e| e.path()).find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(&sticker.id.to_string()))
            });
            let path = match existing {
                Some(path) => path,
                None => {
                    let bytes = manager.sticker_image(id, key, sticker.id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                    Self::save_sticker_image(&dir, sticker.id, &bytes).context("couldn't save a sticker")?
                }
            };
            stickers.push(crate::StickerView { id: sticker.id, emoji: sticker.emoji.clone(), path });
        }
        let cover_id = manifest.cover.as_ref().map(|c| c.id);
        let cover = cover_id.and_then(|c| stickers.iter().find(|s| s.id == c)).or(stickers.first()).map(|s| s.path.clone());
        Ok(StickerPackPreview {
            pack: StickerPackSummary {
                id: id.to_vec(),
                key: key.to_vec(),
                title: manifest.title.clone(),
                author: manifest.author.clone(),
                cover,
                sticker_count: manifest.stickers.len() as u32,
                installed: false,
                blessed: is_blessed(id),
            },
            stickers,
        })
    }

    /// Desktop's `installStickerPack` from the UI: downloaded and saved,
    /// our other devices told (sync message, then Storage Service).
    pub(super) async fn install_sticker_pack_ui(self: &Rc<Self>, id: Vec<u8>, key: Vec<u8>) {
        let Some(mut manager) = self.manager() else { return self.error("can't install the sticker pack", "not connected") };
        if let Err(error) = manager.install_sticker_pack(&id, &key).await {
            return self.error("Sticker pack could not be installed", error);
        }
        let (position, _) = match self.db.install_sticker_pack_row(&hex_of(&id), &hex_of(&key), None, now_millis()).await {
            Ok(installed) => installed,
            Err(error) => return self.error("failed to save the sticker pack", error),
        };
        self.unhide_installed_pack(&id).await;
        self.stickers_changed().await;
        match storage::write_sticker_pack(&manager, &id, &key, position, 0).await {
            Ok(()) => self.storage_written(),
            Err(error) => self.error("couldn't save the sticker pack on your other devices", format!("{error:#}")),
        }
    }

    /// Desktop's `uninstallStickerPack` from the UI.
    pub(super) async fn uninstall_sticker_pack_ui(self: &Rc<Self>, id: Vec<u8>, key: Vec<u8>) {
        let Some(mut manager) = self.manager() else { return self.error("can't remove the sticker pack", "not connected") };
        if let Err(error) = manager.remove_sticker_pack(&id, &key).await {
            return self.error("couldn't remove the sticker pack", error);
        }
        let at = now_millis();
        if let Err(error) = self.db.uninstall_sticker_pack_row(&hex_of(&id), at).await {
            warn!(%error, "failed to save the sticker pack's removal");
        }
        self.stickers_changed().await;
        match storage::write_sticker_pack(&manager, &id, &key, 0, at as u64).await {
            Ok(()) => self.storage_written(),
            Err(error) => self.error("couldn't save the sticker pack on your other devices", format!("{error:#}")),
        }
    }

    /// Installed packs in a new order, saved for every device.
    pub(super) async fn order_sticker_packs(self: &Rc<Self>, ids: Vec<Vec<u8>>) {
        let hex: Vec<String> = ids.iter().map(|id| hex_of(id)).collect();
        if let Err(error) = self.db.set_sticker_pack_order(&hex).await {
            return self.error("failed to save the sticker packs' order", error);
        }
        self.stickers_changed().await;
        let Some(manager) = self.manager() else { return };
        let installed = self.load_stickers().await;
        for (position, id) in ids.iter().enumerate() {
            let Some(pack) = installed.iter().find(|p| &p.id == id) else { continue };
            if let Err(error) = storage::write_sticker_pack(&manager, &pack.id, &pack.key, position as u32, 0).await {
                return self.error("couldn't save the sticker packs' order on your other devices", format!("{error:#}"));
            }
        }
        self.storage_written();
    }

    /// Our other device installed or removed packs: presage downloads or
    /// removes them; the order and the manager follow.
    pub(super) async fn apply_sticker_pack_sync(self: &Rc<Self>, packs: Vec<(Vec<u8>, Option<Vec<u8>>)>) {
        for (id, key) in packs {
            if key.is_some() {
                self.unhide_installed_pack(&id).await;
            }
            let result = match key {
                Some(key) => self.db.install_sticker_pack_row(&hex_of(&id), &hex_of(&key), None, now_millis()).await.map(|_| ()),
                None => self.db.uninstall_sticker_pack_row(&hex_of(&id), now_millis()).await.map(|_| ()),
            };
            if let Err(error) = result {
                warn!(%error, "failed to save a synced sticker pack");
            }
        }
        self.stickers_changed().await;
    }

    /// Desktop's `maybeUnhideStickerPack`, when a pack is installed (here,
    /// by sync or from Storage Service): a hidden pack is shown again, and
    /// its tombstone written.
    async fn unhide_installed_pack(self: &Rc<Self>, id: &[u8]) {
        match self.db.unhide_sticker_pack(&hex_of(id), now_millis()).await {
            Ok(Some(tombstone)) => {
                self.note_hidden_pack_change();
                let this = self.clone();
                tokio::task::spawn_local(async move { this.write_hidden_pack_now(&tombstone).await });
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "failed to unhide an installed sticker pack"),
        }
    }

    fn note_hidden_pack_change(&self) {
        let changes = &self.favorite_downloads.hidden_pack_changes;
        changes.set(changes.get() + 1);
    }

    /// Writes a hidden pack's record (or tombstone) and marks it written;
    /// one that fails is written again at the next sync.
    async fn write_hidden_pack_now(&self, pack: &langis_store::HiddenStickerPack) {
        let Some(manager) = self.manager() else { return };
        match self.write_hidden_pack(&manager, pack).await {
            Ok(()) => self.storage_written(),
            Err(error) => warn!(error = %format!("{error:#}"), "couldn't write a hidden sticker pack's record yet"),
        }
    }

    async fn write_hidden_pack(&self, manager: &SignalManager, pack: &langis_store::HiddenStickerPack) -> anyhow::Result<()> {
        let id = hex::decode(&pack.pack_id)?;
        let key = pack.pack_key.as_deref().map(hex::decode).transpose()?.unwrap_or_default();
        let unhidden_at = pack.unhidden_at.unwrap_or(0).max(0) as u64;
        storage::write_hidden_sticker_pack(manager, &id, &key, unhidden_at).await?;
        self.db.hidden_sticker_pack_written(pack).await?;
        // No longer pending, so a sync whose snapshot came before this
        // write would undo it: it counts as a change too.
        self.note_hidden_pack_change();
        Ok(())
    }

    /// Desktop's `markStickerPackAsHidden` (commits 5edf85d87, 8049395d9):
    /// hiding uninstalls an installed pack first, takes the pack's key,
    /// and writes the record; showing it again writes its tombstone.
    pub(super) async fn set_sticker_pack_hidden(self: &Rc<Self>, id: Vec<u8>, key: Vec<u8>, hidden: bool) {
        let hex = hex_of(&id);
        let changed = if hidden {
            if self.load_stickers().await.iter().any(|p| p.id == id) {
                self.uninstall_sticker_pack_ui(id.clone(), key.clone()).await;
            }
            // The key we know the pack by, over the one the request names.
            let rows = self.db.sticker_pack_rows().await.unwrap_or_default();
            let key_hex = rows.into_iter().find(|r| r.id == hex).map_or_else(|| hex_of(&key), |r| r.key);
            self.db.hide_sticker_pack(&hex, &key_hex).await
        } else {
            self.db.unhide_sticker_pack(&hex, now_millis()).await
        };
        let pack = match changed {
            Ok(pack) => pack,
            Err(error) => return self.error("failed to save the hidden sticker pack", error),
        };
        if let Some(pack) = &pack {
            self.note_hidden_pack_change();
            self.emit(Event::StickerManager(self.sticker_manager_view().await));
            self.emit(Event::StickerPackHidden { id: id.clone(), key: key.clone(), hidden });
            self.write_hidden_pack_now(pack).await;
        }
    }

    /// Storage Service's hidden packs, merged (Desktop's
    /// `mergeHiddenStickerPackRecord`; tombstones older than the message
    /// queue time go), then what's left to write written (at most
    /// [`HIDDEN_WRITES_PER_SYNC`]). `changes` is the count of local changes
    /// from before the snapshot was read, as for favorites.
    pub(super) async fn apply_storage_hidden_sticker_packs(self: &Rc<Self>, remote: Vec<langis_store::RemoteHiddenStickerPack>, changes: u64) {
        if self.favorite_downloads.hidden_pack_changes.get() != changes {
            return info!("hidden sticker packs changed during the sync; merging them at the next");
        }
        let drop_before = now_millis() - crate::remote_config().message_queue_time_ms();
        let merge = match self.db.merge_storage_hidden_packs(&remote, drop_before).await {
            Ok(merge) => merge,
            Err(error) => return warn!(%error, "failed to save hidden sticker packs"),
        };
        if merge.keyless > 0 {
            warn!(skipped = merge.keyless, "hidden sticker pack records without a pack key skipped");
        }
        if merge.changed {
            self.emit(Event::StickerManager(self.sticker_manager_view().await));
        }
        let Some(manager) = self.manager() else { return };
        let pending: Vec<_> = self
            .db
            .hidden_sticker_packs()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.needs_sync)
            .take(HIDDEN_WRITES_PER_SYNC)
            .collect();
        let mut written = false;
        for pack in &pending {
            match self.write_hidden_pack(&manager, pack).await {
                Ok(()) => written = true,
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "couldn't write a hidden sticker pack's record");
                    break;
                }
            }
        }
        if written {
            self.storage_written();
        }
    }

    pub(super) async fn stickers_changed(self: &Rc<Self>) {
        self.emit(Event::Stickers(self.load_stickers().await));
        self.emit(Event::StickerManager(self.sticker_manager_view().await));
        // Favorites follow their pack: an uninstalled pack's need their
        // own images.
        self.favorite_stickers_changed().await;
    }

    /// Storage Service's packs (Desktop's `mergeInstalledStickerPackRecord`):
    /// one uninstalled elsewhere goes; one installed elsewhere downloads
    /// (one at a time, in the background); an installed one takes its
    /// position.
    pub(super) async fn apply_storage_sticker_packs(self: &Rc<Self>, remote: Vec<storage::StorageStickerPack>) {
        let installed: Vec<Vec<u8>> = match self.store.sticker_packs().await {
            Ok(packs) => packs.flatten().map(|p| p.id).collect(),
            Err(_) => return,
        };
        let mut to_install = Vec::new();
        let mut changed = false;
        for pack in remote {
            let hex = hex_of(&pack.id);
            match (pack.deleted_at, pack.key) {
                (Some(at), _) => {
                    if installed.contains(&pack.id) {
                        let mut store = self.store.clone();
                        if let Err(error) = store.remove_sticker_pack(&pack.id).await {
                            warn!(%error, "failed to remove a sticker pack");
                            continue;
                        }
                        changed = true;
                    }
                    let _ = self.db.uninstall_sticker_pack_row(&hex, at).await;
                }
                (None, Some(key)) if !installed.contains(&pack.id) => {
                    self.unhide_installed_pack(&pack.id).await;
                    to_install.push((pack.id, key, pack.position))
                }
                (None, Some(key)) => {
                    let _ = self.db.install_sticker_pack_row(&hex, &hex_of(&key), Some(pack.position), now_millis()).await;
                    changed = true;
                }
                (None, None) => {}
            }
        }
        if changed {
            self.stickers_changed().await;
        }
        if to_install.is_empty() || INSTALLING.replace(true) {
            return;
        }
        let this = self.clone();
        tokio::task::spawn_local(async move {
            if let Some(mut manager) = this.manager() {
                for (id, key, position) in to_install {
                    match manager.download_sticker_pack(&id, &key).await {
                        Ok(_) => {
                            let _ = this.db.install_sticker_pack_row(&hex_of(&id), &hex_of(&key), Some(position), now_millis()).await;
                            this.stickers_changed().await;
                        }
                        Err(error) => warn!(%error, "couldn't download a sticker pack installed elsewhere"),
                    }
                }
            }
            INSTALLING.set(false);
        });
    }

    /// A `signal.art` link's preview (Desktop's `getStickerPackPreview`):
    /// the pack's title, "Sticker Pack by {author}" and its cover.
    pub(super) async fn sticker_pack_link_preview(&self, url: &str) -> Option<crate::LinkPreviewDraft> {
        let (id, key) = crate::stickers::parse_pack_link(url)?;
        let mut manager = self.manager()?;
        let manifest = manager.sticker_pack_manifest(&id, &key).await.ok()?;
        let cover_id = manifest.cover.as_ref().map(|c| c.id).or_else(|| manifest.stickers.first().map(|s| s.id))?;
        let bytes = manager.sticker_image(&id, &key, cover_id).await.ok()?;
        let (kind, width, height) = media::probe(&bytes).unwrap_or((media::ImageKind::Webp, 512, 512));
        Some(crate::LinkPreviewDraft {
            url: url.to_owned(),
            title: manifest.title.clone(),
            description: Some(format!("Sticker Pack by {}", manifest.author)),
            date: None,
            image: Some(crate::PreviewImage { bytes: std::sync::Arc::new(bytes), content_type: kind.content_type(), width, height }),
        })
    }
}
