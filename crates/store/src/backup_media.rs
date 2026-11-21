//! Desktop's backup media download (`ts/util/backupMediaDownload.preload.ts`
//! and the `attachment_downloads_backup_stats` table): the attachments of a
//! transferred backup queued to download from the backup tier, and how many
//! of their bytes are done. Desktop counts each job's expected ciphertext
//! size into the total when it's queued and into the bytes done when it
//! finishes, whether it downloaded or gave up.

use crate::{AppDb, Result};

/// Settings key: the bytes of finished jobs since the counts were last
/// reset (Desktop's `completedBytes`).
const COMPLETED: &str = "backup.media_download.completed";

/// How far the backup media download is (Desktop's
/// `BackupAttachmentDownloadProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackupMediaProgress {
    /// Bytes done plus bytes still queued.
    pub total_bytes: u64,
    pub completed_bytes: u64,
    /// Jobs still queued.
    pub pending: u64,
}

impl AppDb {
    /// Queues attachments to download from the backup tier: `(message id,
    /// attachment index, bytes)`. One already queued is left as it is.
    pub async fn queue_backup_media(&self, jobs: &[(i64, usize, u64)]) -> Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let mut tx = self.write().await?;
        for (message_id, index, bytes) in jobs {
            sqlx::query("INSERT OR IGNORE INTO backup_media_downloads (message_id, idx, bytes) VALUES (?, ?, ?)")
                .bind(message_id)
                .bind(*index as i64)
                .bind(i64::try_from(*bytes).unwrap_or(i64::MAX))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The next jobs, newest message first (Desktop's
    /// `getNextAttachmentDownloadJobs` orders by `receivedAt DESC`).
    pub async fn next_backup_media(&self, limit: u32) -> Result<Vec<(i64, usize)>> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT message_id, idx FROM backup_media_downloads ORDER BY message_id DESC, idx LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(m, i)| (m, i as usize)).collect())
    }

    /// A job is over (downloaded, or given up): its bytes count as done.
    /// Returns false when it wasn't queued (canceled meanwhile).
    pub async fn finish_backup_media(&self, message_id: i64, index: usize) -> Result<bool> {
        let mut tx = self.write().await?;
        let bytes: Option<i64> =
            sqlx::query_scalar("DELETE FROM backup_media_downloads WHERE message_id = ? AND idx = ? RETURNING bytes")
                .bind(message_id)
                .bind(index as i64)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(bytes) = bytes else {
            return Ok(false);
        };
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + ?2 AS TEXT)",
        )
        .bind(COMPLETED)
        .bind(bytes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn backup_media_progress(&self) -> Result<BackupMediaProgress> {
        let (pending, queued): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM backup_media_downloads").fetch_one(&self.pool).await?;
        let completed = self.setting(COMPLETED).await?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        Ok(BackupMediaProgress {
            total_bytes: completed.saturating_add(queued.max(0) as u64),
            completed_bytes: completed,
            pending: pending.max(0) as u64,
        })
    }

    /// Desktop's `resetBackupAttachmentDownloadStats`: the bytes done start
    /// again from nothing; queued jobs stay.
    pub async fn reset_backup_media_progress(&self) -> Result<()> {
        self.remove_setting(COMPLETED).await
    }

    /// Desktop's `cancelBackupMediaDownload`: every job dropped and the
    /// counts reset.
    pub async fn cancel_backup_media(&self) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM backup_media_downloads").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM settings WHERE key = ?").bind(COMPLETED).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets the counts outright, for demo data.
    pub async fn seed_backup_media_progress(&self, completed: u64, queued: &[(i64, usize, u64)]) -> Result<()> {
        self.cancel_backup_media().await?;
        self.queue_backup_media(queued).await?;
        self.set_setting(COMPLETED, &completed.to_string()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jobs_count_into_the_total_and_finish_into_the_bytes_done() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = AppDb::open(&dir.join("app.db"), &[3; 32]).await.unwrap();
        std::mem::forget(db.clone());
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress::default());
        db.queue_backup_media(&[(1, 0, 100), (2, 0, 50), (2, 1, 25)]).await.unwrap();
        // Queued twice counts once.
        db.queue_backup_media(&[(1, 0, 100)]).await.unwrap();
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress { total_bytes: 175, completed_bytes: 0, pending: 3 });
        assert_eq!(db.next_backup_media(2).await.unwrap(), vec![(2, 0), (2, 1)]);
        assert!(db.finish_backup_media(2, 0).await.unwrap());
        assert!(!db.finish_backup_media(2, 0).await.unwrap());
        assert!(db.finish_backup_media(1, 0).await.unwrap());
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress { total_bytes: 175, completed_bytes: 150, pending: 1 });
        // A reset forgets what's done, not what's queued.
        db.reset_backup_media_progress().await.unwrap();
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress { total_bytes: 25, completed_bytes: 0, pending: 1 });
        db.cancel_backup_media().await.unwrap();
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress::default());
        db.seed_backup_media_progress(10, &[(5, 0, 30)]).await.unwrap();
        assert_eq!(db.backup_media_progress().await.unwrap(), BackupMediaProgress { total_bytes: 40, completed_bytes: 10, pending: 1 });
    }
}
