-- Desktop's per-chat "While muted" notifications and unread reminders
-- (`notifyForCallsIfMuted`, `notifyForMentionsIfMuted`,
-- `notifyForRepliesIfMuted`, `showUnreadReminders`): 1 on, 0 off, NULL
-- follows the account setting. A chat set to "Mentions: Don't notify"
-- starts with mentions off, as Desktop's
-- `resolveLegacyNotifyForMentionsIfMuted` reads it.
ALTER TABLE conversations ADD COLUMN notify_calls_if_muted INTEGER;
ALTER TABLE conversations ADD COLUMN notify_mentions_if_muted INTEGER;
ALTER TABLE conversations ADD COLUMN notify_replies_if_muted INTEGER;
ALTER TABLE conversations ADD COLUMN show_unread_reminders INTEGER;
UPDATE conversations SET notify_mentions_if_muted = 0 WHERE dont_notify_mentions = 1;
