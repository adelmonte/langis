//! Reads for writing a backup (`langis_core`'s Desktop backups).

use sqlx::AssertSqlSafe;

use crate::{AppDb, ConversationId, Message, Result, message_from_row, MESSAGE_COLUMNS};

impl AppDb {
    /// A chat's messages oldest first, `limit` at a time: the first page
    /// with `after` `None`, then each after the last one's id. Notices
    /// included. Walks the timeline index (`conversation_id, sent_at, id`).
    pub async fn messages_in_order(&self, conversation: &ConversationId, after: Option<i64>, limit: u32) -> Result<Vec<Message>> {
        let rows = match after {
            Some(after) => {
                sqlx::query(AssertSqlSafe(format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1 \
                       AND (sent_at, id) > (SELECT sent_at, id FROM messages WHERE id = ?2) \
                     ORDER BY sent_at, id LIMIT ?3"
                )))
                .bind(&conversation.0)
                .bind(after)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(AssertSqlSafe(format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1 ORDER BY sent_at, id LIMIT ?2"
                )))
                .bind(&conversation.0)
                .bind(i64::from(limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(message_from_row).collect()
    }

    /// When a message was last edited (its latest edit's timestamp), or
    /// `None` when it never was.
    pub async fn latest_edit(&self, message_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MAX(sent_at) FROM message_edits WHERE message_id = ?")
            .bind(message_id)
            .fetch_one(&self.pool)
            .await?)
    }
}
