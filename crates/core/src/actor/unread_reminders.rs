//! Desktop's unread reminders (`ts/services/unreadReminders.preload.ts`):
//! hourly, the muted chat whose unread messages have waited longest, once
//! they're three days old, as `Event::UnreadReminder`. The frontend shows it
//! and answers `Command::UnreadReminderShown`, which starts the chat's next
//! three-day wait (Desktop marks it when its notification is shown).

use std::time::Duration;

use langis_store::{
    ChatNotify, ChatNotifyKind, ConversationKind, UNREAD_REMINDER_MIN_INTERVAL, UNREAD_REMINDER_POLL_INTERVAL,
    UNREAD_REMINDERS_ENABLED_AT,
};

use super::*;

impl Actor {
    pub(super) async fn unread_reminder_loop(self: Rc<Self>) {
        // Desktop's `init`: only messages from now on are reminded of.
        if let Ok(None) = self.db.setting(UNREAD_REMINDERS_ENABLED_AT).await {
            let _ = self.db.set_setting(UNREAD_REMINDERS_ENABLED_AT, &now_millis().to_string()).await;
        }
        loop {
            if let Err(error) = self.check_unread_reminders().await {
                tracing::warn!("unread reminders: {error:#}");
            }
            tokio::time::sleep(Duration::from_millis(UNREAD_REMINDER_POLL_INTERVAL as u64)).await;
        }
    }

    /// Desktop's `doRefresh`: one reminder at most.
    async fn check_unread_reminders(&self) -> anyhow::Result<()> {
        let now = now_millis();
        let mut account = ChatNotify::default();
        for kind in ChatNotifyKind::ALL {
            let on = Some(self.setting_on(crate::AccountSetting::for_chat(kind)).await);
            match kind {
                ChatNotifyKind::Calls => account.calls = on,
                ChatNotifyKind::Mentions => account.mentions = on,
                ChatNotifyKind::Replies => account.replies = on,
                ChatNotifyKind::UnreadReminders => account.unread_reminders = on,
            }
        }
        let on = |chat: &langis_store::Conversation, kind| chat.notify.on(kind, |k| account.get(k) == Some(true));
        let enabled_at = self.db.setting(UNREAD_REMINDERS_ENABLED_AT).await?.and_then(|v| v.parse().ok()).unwrap_or(i64::MAX);
        let reminded = self.db.unread_reminded_at().await?;
        let chats = self.db.conversations().await?;
        // Desktop's `isEligible`.
        let eligible = |chat: &langis_store::Conversation| {
            chat.kind != ConversationKind::NoteToSelf
                && !chat.left
                && !chat.blocked
                && !chat.archived
                && chat.muted
                && on(chat, ChatNotifyKind::UnreadReminders)
                && reminded.get(&chat.id).is_none_or(|at| *at <= now - UNREAD_REMINDER_MIN_INTERVAL)
                && chat.unread_count > 0
        };
        let since: Vec<_> =
            chats.iter().filter(|c| eligible(c)).map(|c| (c.id.clone(), reminded.get(&c.id).copied().unwrap_or(enabled_at))).collect();
        if since.is_empty() {
            return Ok(());
        }
        let ranges = self.db.unreminded_unread_ranges(&since).await?;
        let me = self.self_aci();
        for conversation in langis_store::due_reminders(&ranges, now) {
            let Some(chat) = chats.iter().find(|c| c.id == conversation) else { continue };
            let mut reminder = self.db.unread_reminder_data(&conversation, &me).await?;
            if reminder.unread == 0 {
                continue;
            }
            // Desktop's `getSummary`: counts only for kinds on while muted.
            if !on(chat, ChatNotifyKind::Mentions) {
                reminder.mentions = 0;
            }
            if !on(chat, ChatNotifyKind::Replies) {
                reminder.replies = 0;
            }
            self.emit(Event::UnreadReminder { conversation, reminder });
            return Ok(());
        }
        Ok(())
    }
}
