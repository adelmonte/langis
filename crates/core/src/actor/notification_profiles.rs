//! Notification profiles, as Desktop syncs them: each change saved here
//! and then as the profile's `NotificationProfile` record or the account
//! record's override (`ts/state/ducks/notificationProfiles.preload.ts`,
//! which runs a storage upload after each), and records read back with
//! `mergeNotificationProfileRecord` and `mergeAccountRecord`.

use langis_store::{NotificationProfile, NotificationProfileEdit};

use super::*;
use crate::storage::notification_profiles::{self as remote, RemoteProfiles};

impl Actor {
    pub(super) async fn edit_notification_profiles(self: &Rc<Self>, edit: NotificationProfileEdit) {
        let changes = match self.db.edit_notification_profiles(edit).await {
            Ok(changes) => changes,
            Err(error) => return self.error("failed to save notification profiles", error),
        };
        if changes.is_empty() {
            return;
        }
        self.emit(Event::NotificationProfilesChanged);
        let Some(manager) = self.manager() else { return };
        let profiles: Vec<NotificationProfile> = match self.db.notification_profiles().await {
            Ok(profiles) => profiles.into_iter().filter(|p| changes.profiles.contains(&p.id)).collect(),
            Err(error) => return self.error("failed to read notification profiles", error),
        };
        for profile in profiles {
            if let Err(error) = remote::write_profile(&manager, &profile).await {
                return self.error("couldn't save the notification profile on your other devices", format!("{error:#}"));
            }
        }
        if changes.override_ {
            let override_ = match self.db.notification_profile_override().await {
                Ok(o) => o,
                Err(error) => return self.error("failed to read notification profiles", error),
            };
            if let Err(error) = remote::write_override(&manager, override_.as_ref()).await {
                return self.error("couldn't turn the notification profile on or off on your other devices", format!("{error:#}"));
            }
        }
        self.storage_written();
    }

    /// Storage Service's profiles and override merged.
    pub(super) async fn apply_storage_notification_profiles(self: &Rc<Self>, remote: RemoteProfiles) {
        let now = now_millis().max(0) as u64;
        let mut edits: Vec<NotificationProfileEdit> = remote.profiles.into_iter().map(NotificationProfileEdit::Merge).collect();
        if let Some(override_) = remote.override_ {
            edits.push(NotificationProfileEdit::MergeOverride { override_, at: now });
        }
        let mut changed = false;
        for edit in edits {
            match self.db.edit_notification_profiles(edit).await {
                Ok(changes) => changed |= !changes.is_empty(),
                Err(error) => return self.error("failed to save notification profiles", error),
            }
        }
        if changed {
            self.emit(Event::NotificationProfilesChanged);
        }
    }
}
