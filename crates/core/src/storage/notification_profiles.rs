//! Notification profiles in Storage Service: each profile's
//! `NotificationProfile` record (Desktop's `toNotificationProfileRecord`
//! and `mergeNotificationProfileRecord`) and the account record's
//! `notificationProfileManualOverride` (`toAccountRecord`,
//! `mergeAccountRecord`), in `ts/services/storageRecordOps.preload.ts`.

use anyhow::{Context as _, bail};
use langis_store::{NotificationProfile, NotificationProfileOverride};
use presage::proto::account_record::{NotificationProfileManualOverride, notification_profile_manual_override as manual};
use presage::proto::{StorageRecord, manifest_record::identifier::Type as IdentifierType, storage_record::Record};
use prost::Message as _;

use super::{READ_BATCH, Session, chat_recipient, edit_inner, manifest_ikm, recipient_chat, set_bytes, write_account_with};
use crate::actor::SignalManager;

const RECORD_NOTIFICATION_PROFILE: u32 = 9; // StorageRecord.notificationProfile
/// NotificationProfile's fields after its id (1): name (2) to
/// deletedAtTimestampMs (13).
const PROFILE_FIELDS: std::ops::RangeInclusive<u32> = 2..=13;
const RECORD_ACCOUNT: u32 = 4; // StorageRecord.account
/// AccountRecord.notificationProfileManualOverride.
const ACCOUNT_OVERRIDE: u32 = 44;

/// What Storage Service has for notification profiles.
#[derive(Debug, Default)]
pub(crate) struct RemoteProfiles {
    /// Every profile record, deleted ones too.
    pub profiles: Vec<NotificationProfile>,
    /// The account record's override; `None` without an account record.
    pub override_: Option<Option<NotificationProfileOverride>>,
}

/// A profile record as ours; none without a 16-byte id or a name, which
/// Desktop drops.
pub(crate) fn profile(record: &presage::proto::NotificationProfile) -> Option<NotificationProfile> {
    if record.id.len() != langis_store::NOTIFICATION_PROFILE_ID_LENGTH || record.name.is_empty() {
        return None;
    }
    Some(NotificationProfile {
        id: hex::encode(&record.id),
        name: record.name.clone(),
        emoji: record.emoji.clone(),
        color: if record.color == 0 { langis_store::DEFAULT_PROFILE_COLOR } else { record.color },
        created_at: record.created_at_ms,
        allow_all_calls: record.allow_all_calls,
        allow_all_mentions: record.allow_all_mentions,
        allowed_members: record.allowed_members.iter().filter_map(recipient_chat).collect(),
        schedule_enabled: record.schedule_enabled,
        schedule_start: Some(record.schedule_start_time),
        schedule_end: Some(record.schedule_end_time),
        schedule_days: NotificationProfile::days_from_numbers(record.schedule_days_enabled.iter().copied()),
        deleted_at: record.deleted_at_timestamp_ms,
    })
}

/// The account record's override as ours (`set_at` is filled in when it's
/// merged).
pub(crate) fn manual_override(record: Option<&NotificationProfileManualOverride>) -> Option<NotificationProfileOverride> {
    match record?.r#override.as_ref()? {
        manual::Override::Enabled(on) if !on.id.is_empty() => Some(NotificationProfileOverride::Enabled {
            id: hex::encode(&on.id),
            ends_at: Some(on.end_at_timestamp_ms).filter(|t| *t > 0),
            set_at: 0,
        }),
        manual::Override::DisabledAtTimestampMs(at) if *at > 0 => Some(NotificationProfileOverride::Disabled { at: *at }),
        _ => None,
    }
}

fn encode_override(override_: Option<&NotificationProfileOverride>) -> Vec<u8> {
    let value = match override_ {
        None => return Vec::new(),
        Some(NotificationProfileOverride::Disabled { at }) => manual::Override::DisabledAtTimestampMs(*at),
        Some(NotificationProfileOverride::Enabled { id, ends_at, .. }) => manual::Override::Enabled(manual::ManuallyEnabled {
            id: hex::decode(id).unwrap_or_default(),
            end_at_timestamp_ms: ends_at.unwrap_or(0),
        }),
    };
    NotificationProfileManualOverride { r#override: Some(value) }.encode_to_vec()
}

/// Sets a profile record's fields to `profile`'s, keeping its id and any
/// fields this client doesn't know. `Ok(None)` when nothing changes.
pub(crate) fn edit_profile(record: &[u8], profile: &NotificationProfile) -> anyhow::Result<Option<Vec<u8>>> {
    edit_inner(record, RECORD_NOTIFICATION_PROFILE, |inner| {
        let fields = super::wire::fields(inner).context("malformed notification profile record")?;
        let mut out = Vec::with_capacity(inner.len() + 64);
        for field in fields.iter().filter(|f| !PROFILE_FIELDS.contains(&f.number)) {
            out.extend_from_slice(field.raw);
        }
        let known = presage::proto::NotificationProfile {
            id: Vec::new(),
            name: profile.name.clone(),
            emoji: profile.emoji.clone(),
            color: profile.color,
            created_at_ms: profile.created_at,
            allow_all_calls: profile.allow_all_calls,
            allow_all_mentions: profile.allow_all_mentions,
            allowed_members: profile.allowed_members.iter().filter_map(chat_recipient).collect(),
            schedule_enabled: profile.schedule_enabled,
            schedule_start_time: profile.schedule_start.unwrap_or(0),
            schedule_end_time: profile.schedule_end.unwrap_or(0),
            schedule_days_enabled: profile.day_numbers().into_iter().map(i32::from).collect(),
            deleted_at_timestamp_ms: profile.deleted_at,
        };
        out.extend_from_slice(&known.encode_to_vec());
        let decode = |bytes: &[u8]| {
            presage::proto::NotificationProfile::decode(bytes).ok().and_then(|r| profile_with_id(&r, &profile.id))
        };
        let same = decode(inner).is_some_and(|old| old == *profile);
        Ok((!same).then_some(out))
    })
}

/// [`profile`], for comparing a record whose id field was left out.
fn profile_with_id(record: &presage::proto::NotificationProfile, id: &str) -> Option<NotificationProfile> {
    let mut record = record.clone();
    record.id = hex::decode(id).ok()?;
    profile(&record)
}

fn new_record(id: &[u8]) -> Vec<u8> {
    let profile = presage::proto::NotificationProfile { id: id.to_vec(), ..Default::default() };
    StorageRecord { record: Some(Record::NotificationProfile(profile)) }.encode_to_vec()
}

fn is_profile(record: &StorageRecord, id: &[u8]) -> bool {
    matches!(&record.record, Some(Record::NotificationProfile(p)) if p.id == id)
}

/// Saves a profile for the whole account, adding its record if Storage
/// Service doesn't have it.
pub(crate) async fn write_profile(manager: &SignalManager, profile: &NotificationProfile) -> anyhow::Result<()> {
    let id = hex::decode(&profile.id).context("not a notification profile id")?;
    let session = Session::new(manager).await?;
    let kind = IdentifierType::NotificationProfile;
    for _ in 0..3 {
        let manifest = session.manifest().await?;
        let ikm = manifest_ikm(&manifest);
        let keys: Vec<Vec<u8>> =
            manifest.identifiers.iter().filter(|key| key.r#type() == kind).map(|key| key.raw.clone()).collect();
        let mut existing = None;
        for batch in keys.chunks(READ_BATCH) {
            existing = session
                .read_many(batch, ikm)
                .await?
                .into_iter()
                .find(|(_, raw)| StorageRecord::decode(raw.as_slice()).is_ok_and(|record| is_profile(&record, &id)));
            if existing.is_some() {
                break;
            }
        }
        let (old_key, record) = match existing {
            Some((key, raw)) => (Some(key), raw),
            None => (None, new_record(&id)),
        };
        let Some(record) = edit_profile(&record, profile)? else { return Ok(()) };
        if session.commit(manager, &manifest, old_key.as_deref(), kind, &record).await? {
            return Ok(());
        }
    }
    bail!("storage kept changing on another device; try again")
}

/// Sets the account record's override, keeping every other field.
pub(crate) fn edit_account_override(record: &[u8], override_: Option<&NotificationProfileOverride>) -> anyhow::Result<Option<Vec<u8>>> {
    let encoded = encode_override(override_);
    edit_inner(record, RECORD_ACCOUNT, |account| set_bytes(account, ACCOUNT_OVERRIDE, &encoded))
}

/// Saves the override in the account record, for the whole account.
pub(crate) async fn write_override(manager: &SignalManager, override_: Option<&NotificationProfileOverride>) -> anyhow::Result<()> {
    write_account_with(manager, |record| edit_account_override(record, override_)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use langis_store::ConversationId;

    #[test]
    fn profile_records_read_and_write_as_desktops() {
        let id = [7u8; 16];
        let mut ours = NotificationProfile::new(hex::encode(id), "Work".into(), 1234);
        ours.emoji = Some("💪".into());
        ours.allowed_members = vec![ConversationId::group(&[9; 32])];
        ours.schedule_enabled = true;
        let mut inner = presage::proto::NotificationProfile { id: id.to_vec(), ..Default::default() }.encode_to_vec();
        // A field from a newer client survives.
        super::super::wire::put_varint_field(&mut inner, 99, 5);
        let mut record = Vec::new();
        super::super::wire::put_len(&mut record, RECORD_NOTIFICATION_PROFILE, &inner);
        let edited = edit_profile(&record, &ours).unwrap().expect("changed");
        assert!(edit_profile(&edited, &ours).unwrap().is_none(), "already so");
        let Some(Record::NotificationProfile(written)) = StorageRecord::decode(edited.as_slice()).unwrap().record else { panic!() };
        assert_eq!(written.schedule_days_enabled, vec![1, 2, 3, 4, 5]);
        assert_eq!(profile(&written), Some(ours));
        let inner = super::super::wire::fields(&edited).unwrap()[0].payload.unwrap().to_vec();
        assert!(super::super::wire::fields(&inner).unwrap().iter().any(|f| f.number == 99), "unknown field kept");
        // Desktop drops records without a name.
        assert_eq!(profile(&presage::proto::NotificationProfile { id: id.to_vec(), ..Default::default() }), None);
    }

    #[test]
    fn overrides_round_trip_through_the_account_record() {
        let on = NotificationProfileOverride::Enabled { id: hex::encode([1u8; 16]), ends_at: Some(99), set_at: 0 };
        let off = NotificationProfileOverride::Disabled { at: 42 };
        for o in [on, off] {
            let bytes = encode_override(Some(&o));
            let record = NotificationProfileManualOverride::decode(bytes.as_slice()).unwrap();
            assert_eq!(manual_override(Some(&record)), Some(o));
        }
        assert!(encode_override(None).is_empty());
        assert_eq!(manual_override(None), None);
    }
}
