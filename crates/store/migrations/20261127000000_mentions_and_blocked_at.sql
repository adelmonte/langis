-- Desktop's per-chat "Mentions: Don't notify" while muted
-- (`dontNotifyForMentionsIfMuted`), and when a chat was blocked here.
ALTER TABLE conversations ADD COLUMN dont_notify_mentions INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN blocked_at INTEGER;
