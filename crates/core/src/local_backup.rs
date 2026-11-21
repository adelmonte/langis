//! Desktop backups (Signal Desktop's "local backups", Settings > Backups):
//! an end-to-end encrypted backup in a folder on this computer, which the
//! phone can restore ("Restore or transfer", "On Device Backup").
//!
//! The write side of `history.rs`, laid out and encrypted as Desktop does
//! (`ts/services/backups/index.preload.ts` `exportLocalBackup`,
//! `util/localBackup.node.ts`, `jobs/AttachmentLocalBackupManager.preload.ts`):
//!
//! ```text
//! <folder>/SignalBackups/
//!   signal-backup-YYYY-MM-DD-HH-MM-SS/   one per backup, the newest two kept
//!     main       the backup: BackupInfo and frames, gzipped with flushes and
//!                padded as Desktop does, AES-256-CBC
//!                and HMAC-SHA256 with the message backup key derived from
//!                the account entropy pool's backup key and our backup id
//!     metadata   `signal.backup.local.Metadata`: the version and the
//!                backup id, AES-256-CTR with the local metadata key
//!     files      `FilesFrame`s naming every file the backup refers to
//!   files/<2 hex>/<media name>  each attachment, encrypted as an attachment
//!                with its own `localKey` (in its `FilePointer`), named
//!                hex(sha256(plaintextHash || localKey)), shared by snapshots
//! ```
//!
//! What's written comes from the app database, so it holds what this
//! device has: the chats, people and groups it knows, messages and the
//! notices it shows, downloaded files. What isn't (calls in groups, shared
//! contacts, payments, the contact-removed notice) is counted in
//! [`Stats::skipped`]. libsignal's validator checks every backup before
//! it's kept, as Desktop's export does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use libsignal_message_backup::proto::backup as proto;
use presage::libsignal_service::protocol::Aci;
use langis_store::{AppDb, Conversation, ConversationId, ConversationKind, DeliveryStatus, Direction, GroupNotice, Inviter, Message, Notice};

/// Settings key: the backups folder (`.../SignalBackups`), set while
/// Desktop backups are on (Desktop's `localBackupFolder`).
pub const SETTING_FOLDER: &str = "backup.local.folder";
/// Settings key: the last backup, `<unix millis> <snapshot dir>` (Desktop's
/// `lastLocalBackup`).
pub const SETTING_LAST: &str = "backup.local.last";
/// Settings key: [`key_hash`] of the recovery key the user last confirmed
/// (Desktop's `backupKeyViewedHash`). When it differs from the current
/// key's, Desktop shows "Your recovery key has changed" before backing up.
pub const SETTING_KEY_VIEWED: &str = "backup.local.key_viewed";
/// Settings key: the secret each file's `localKey` is derived from, so the
/// same file gets the same name in every backup (hex, 32 bytes).
const SETTING_FILE_SECRET: &str = "backup.local.file_secret";

/// Desktop's `LOCAL_BACKUP_DIR_NAME`.
pub const DIR_NAME: &str = "SignalBackups";
/// Desktop's `LOCAL_BACKUP_SNAPSHOTS_TO_KEEP`.
const SNAPSHOTS_TO_KEEP: usize = 2;
/// Desktop's `LOCAL_BACKUP_VERSION`.
const LOCAL_BACKUP_VERSION: u32 = 1;
/// Desktop's `MIMINUM_DISK_SPACE_FOR_LOCAL_EXPORT`.
const MINIMUM_FREE_BYTES: u64 = 200 << 20;
/// Desktop's room left over the attachments' size before copying them.
const ATTACHMENT_BUFFER_BYTES: u64 = 100 << 20;
/// Desktop's `MINIMUM_LIFETIME`: a message disappearing within a day
/// isn't backed up (`ts/services/backups/util/expiration.std.ts`).
const MINIMUM_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
/// Desktop's `MAX_BACKUP_MESSAGE_BODY_BYTE_LENGTH`.
const MAX_BODY_BYTES: usize = 128 * 1024;

/// The keys a backup needs: what the phone gave this device when it was
/// linked (or sent since in a keys sync).
#[derive(Clone)]
pub(crate) struct AccountKeys {
    pub aci: Aci,
    /// The account entropy pool: the recovery key the user writes down.
    pub entropy_pool: String,
    pub profile_key: [u8; 32],
    /// The backup (media) tier's root key, when the phone gave one.
    pub media_root_key: Option<[u8; 32]>,
}

impl AccountKeys {
    fn backup_key(&self) -> anyhow::Result<libsignal_account_keys::BackupKey> {
        let pool: libsignal_account_keys::AccountEntropyPool =
            self.entropy_pool.parse().map_err(|_| anyhow!("the recovery key isn't valid"))?;
        Ok(libsignal_account_keys::BackupKey::derive_from_account_entropy_pool(&pool))
    }
}

/// How a backup went so far, for [`crate::Event::LocalBackup`]. Desktop's
/// `LocalBackupExportSteps`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalBackupProgress {
    /// Writing chats and messages (Desktop shows "Exporting backup" without
    /// a figure).
    Messages,
    /// Copying files: bytes done of the total.
    Attachments { done: u64, total: u64 },
    /// Done: when it was made and where.
    Complete { at: i64, snapshot: PathBuf },
    /// It failed or was cancelled; nothing of it is kept.
    Failed(LocalBackupError),
}

/// Why a backup failed, sorted as Desktop's `LocalExportErrors`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalBackupError {
    /// Anything else (the text is for logs).
    General(String),
    /// Too little room to start: free `bytes_needed` and try again.
    NotEnoughStorage { bytes_needed: u64 },
    /// The disk filled up while writing.
    RanOutOfStorage { bytes_needed: u64 },
    /// The folder can't be written.
    Permissions,
    /// This device has no recovery key (the phone didn't share it when
    /// linking), so it can't make a backup the phone could read.
    NoRecoveryKey,
    /// `Command::CancelLocalBackup`.
    Cancelled,
}

/// What a backup held, for logs and tests.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub recipients: usize,
    pub chats: usize,
    pub messages: usize,
    pub files: usize,
    /// Things left out, by why (`"group call"`, `"no file or locator"`, ...).
    pub skipped: std::collections::BTreeMap<&'static str, usize>,
}

impl Stats {
    fn skip(&mut self, why: &'static str) {
        *self.skipped.entry(why).or_default() += 1;
    }
}

/// A file to copy into `files/`: the decrypted file at `source`,
/// encrypted with `local_key`.
#[derive(Debug, Clone)]
pub(crate) struct FileJob {
    pub media_name: String,
    pub source: PathBuf,
    pub local_key: [u8; 64],
    pub size: u64,
}

/// The frames of a backup, unencrypted, and the files it refers to.
pub(crate) struct Frames {
    pub plain: Vec<u8>,
    pub files: Vec<FileJob>,
    pub stats: Stats,
}

/// The recovery key as Desktop shows it (`formatBackupKeyForDisplay`):
/// upper case in groups of four; `convert_ambiguous` writes O as `#` and
/// 0 as `=`, as Desktop's key viewer does, so they can't be confused.
pub fn format_key(key: &str, convert_ambiguous: bool) -> String {
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_uppercase).collect();
    let mut out = String::with_capacity(compact.len() + compact.len() / 4);
    for (ix, c) in compact.chars().enumerate() {
        if ix > 0 && ix % 4 == 0 {
            out.push(' ');
        }
        out.push(match c {
            'O' if convert_ambiguous => '#',
            '0' if convert_ambiguous => '=',
            c => c,
        });
    }
    out
}

/// Whether what the user typed is the recovery key (Desktop's confirm
/// step compares both with ambiguous characters converted).
pub fn key_matches(typed: &str, key: &str) -> bool {
    format_key(typed, true) == format_key(key, true)
}

/// Desktop's `getBackupKeyHash`: base64 of the SHA-256 of the key, kept in
/// [`SETTING_KEY_VIEWED`] once the user confirmed it.
pub fn key_hash(key: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(key.as_bytes()))
}

/// The last backup from [`SETTING_LAST`]: when, and its folder.
pub fn parse_last(value: &str) -> Option<(i64, PathBuf)> {
    let (at, dir) = value.split_once(' ')?;
    Some((at.parse().ok()?, PathBuf::from(dir)))
}

/// Desktop's `getTimestampForFolder`: local time, `YYYY-MM-DD-HH-MM-SS`.
fn folder_timestamp(at: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local.timestamp_millis_opt(at).single().unwrap_or_default().format("%Y-%m-%d-%H-%M-%S").to_string()
}

fn uuid_bytes(service_id: &str) -> Option<Vec<u8>> {
    Aci::parse_from_service_id_string(service_id).map(|aci| uuid::Uuid::from(aci).into_bytes().to_vec())
}

/// A phone number as the backup keeps it: digits, no `+`.
fn e164(phone: &str) -> Option<u64> {
    let digits = phone.strip_prefix('+')?;
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then(|| digits.parse().ok()).flatten()
}

fn frame(item: proto::frame::Item) -> proto::Frame {
    proto::Frame { item: Some(item), ..Default::default() }
}

fn put(out: &mut Vec<u8>, message: &impl protobuf::Message) -> anyhow::Result<()> {
    let bytes = message.write_to_bytes()?;
    let mut len = bytes.len();
    while len >= 0x80 {
        out.push((len as u8) | 0x80);
        len >>= 7;
    }
    out.push(len as u8);
    out.extend(bytes);
    Ok(())
}

/// Recipient ids as frames refer to them. Contacts a message names that
/// have no chat are added as they come up, written before that message
/// (Desktop's `#flushPendingRecipients`).
struct Recipients {
    next: u64,
    me: String,
    self_id: u64,
    release_notes: u64,
    by_chat: HashMap<ConversationId, u64>,
    pending: Vec<proto::Frame>,
}

impl Recipients {
    fn take_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// A person's recipient id (`None` for us), adding them if new.
    fn person(&mut self, service_id: Option<&str>) -> Option<u64> {
        let Some(sid) = service_id.filter(|s| *s != self.me) else { return Some(self.self_id) };
        if sid == langis_store::SIGNAL_ACI {
            return Some(self.release_notes);
        }
        let id = ConversationId::direct(sid);
        if let Some(found) = self.by_chat.get(&id) {
            return Some(*found);
        }
        let aci = uuid_bytes(sid)?;
        let recipient_id = self.take_id();
        let contact = proto::Contact {
            aci: Some(aci),
            registration: Some(proto::contact::Registration::Registered(Default::default())),
            ..Default::default()
        };
        let recipient = proto::Recipient {
            id: recipient_id,
            destination: Some(proto::recipient::Destination::Contact(contact)),
            ..Default::default()
        };
        self.pending.push(frame(proto::frame::Item::Recipient(recipient)));
        self.by_chat.insert(id, recipient_id);
        Some(recipient_id)
    }
}

fn flag_on(value: Option<String>, setting: crate::AccountSetting) -> bool {
    match value.as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => setting.default_on(),
    }
}

async fn account_data(db: &AppDb, keys: &AccountKeys) -> proto::AccountData {
    use crate::AccountSetting as S;
    let me = keys.aci.service_id_string();
    let profile = crate::own_profile(db, &me).await;
    let setting = async |key: &str| db.setting(key).await.ok().flatten();
    let flag = async |s: S| flag_on(setting(s.key()).await, s);
    let mut settings = proto::account_data::AccountSettings::new();
    settings.readReceipts = flag(S::ReadReceipts).await;
    settings.sealedSenderIndicators = flag(S::SealedSenderIndicators).await;
    settings.typingIndicators = flag(S::TypingIndicators).await;
    settings.linkPreviews = flag(S::LinkPreviews).await;
    settings.preferContactAvatars = flag(S::PreferContactAvatars).await;
    settings.keepMutedChatsArchived = flag(S::KeepMutedChatsArchived).await;
    settings.notDiscoverableByPhoneNumber = setting(crate::SETTING_PHONE_NUMBER_DISCOVERABLE).await.as_deref() == Some("false");
    settings.universalExpireTimerSeconds =
        setting(crate::SETTING_UNIVERSAL_EXPIRE_TIMER).await.and_then(|v| v.parse().ok()).unwrap_or(0);
    let everybody = setting(crate::SETTING_PHONE_NUMBER_SHARING).await.as_deref() == Some("true");
    use proto::account_data::PhoneNumberSharingMode;
    settings.phoneNumberSharingMode =
        if everybody { PhoneNumberSharingMode::EVERYBODY } else { PhoneNumberSharingMode::NOBODY }.into();
    settings.allowAutomaticKeyVerification = setting(crate::SETTING_KEY_TRANSPARENCY_DISABLED).await.as_deref() != Some("true");
    settings.storiesDisabled = setting(crate::SETTING_STORIES_DISABLED).await.as_deref() == Some("true");
    use proto::account_data::AppTheme;
    settings.appTheme = match setting(crate::SETTING_THEME).await.as_deref() {
        Some("light") => AppTheme::LIGHT,
        Some("dark") => AppTheme::DARK,
        _ => AppTheme::SYSTEM,
    }
    .into();
    if let Some(reactions) = setting(crate::SETTING_PREFERRED_REACTIONS).await.and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
        && crate::preferred_reactions_syncable(&reactions)
    {
        settings.preferredReactionEmoji = reactions;
    }
    // Desktop's defaults for what langis has no setting for.
    settings.defaultSentMediaQuality = proto::account_data::SentMediaQuality::STANDARD.into();
    settings.callsUseLessDataSetting = proto::account_data::CallsUseLessDataSetting::MOBILE_DATA_ONLY.into();
    settings.unreadBadgeType = proto::account_data::account_settings::UnreadBadgeType::UNREAD_MESSAGES.into();
    let mut account = proto::AccountData::new();
    account.profileKey = keys.profile_key.to_vec();
    account.username = profile.username.clone();
    account.givenName = profile.given_name.clone();
    account.familyName = profile.family_name.clone();
    account.bioText = profile.about.clone();
    account.bioEmoji = profile.about_emoji.clone();
    account.accountSettings = Some(settings).into();
    account
}

/// Builds the backup's frames from the app database (Desktop's
/// `BackupExportStream`). `file_secret` names and keys the files; `now`
/// decides which disappearing messages are left out.
pub(crate) async fn frames(db: &AppDb, keys: &AccountKeys, file_secret: &[u8; 32], now: i64) -> anyhow::Result<Frames> {
    let me = keys.aci.service_id_string();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    let mut files: Vec<FileJob> = Vec::new();
    let media_root = keys.media_root_key.unwrap_or_else(rand::random);
    let info = proto::BackupInfo {
        version: 1,
        backupTimeMs: now as u64,
        mediaRootBackupKey: media_root.to_vec(),
        currentAppVersion: format!("langis {}", env!("CARGO_PKG_VERSION")),
        firstAppVersion: format!("langis {}", env!("CARGO_PKG_VERSION")),
        ..Default::default()
    };
    put(&mut out, &info)?;
    put(&mut out, &frame(proto::frame::Item::Account(account_data(db, keys).await)))?;

    let mut chats: Vec<Conversation> = db.conversations().await?;
    chats.extend(db.archived_conversations().await?);
    let mut recipients =
        Recipients { next: 0, me: me.clone(), self_id: 0, release_notes: 0, by_chat: HashMap::new(), pending: Vec::new() };
    recipients.self_id = recipients.take_id();
    recipients.by_chat.insert(ConversationId::direct(&me), recipients.self_id);
    let self_recipient = proto::Recipient {
        id: recipients.self_id,
        destination: Some(proto::recipient::Destination::Self_(Default::default())),
        ..Default::default()
    };
    put(&mut out, &frame(proto::frame::Item::Recipient(self_recipient)))?;
    recipients.release_notes = recipients.take_id();
    recipients.by_chat.insert(ConversationId::signal(), recipients.release_notes);
    let notes = proto::Recipient {
        id: recipients.release_notes,
        destination: Some(proto::recipient::Destination::ReleaseNotes(Default::default())),
        ..Default::default()
    };
    put(&mut out, &frame(proto::frame::Item::Recipient(notes)))?;
    stats.recipients += 2;

    // People and groups with a chat.
    let mut kept: Vec<&Conversation> = Vec::new();
    for chat in &chats {
        if chat.id.as_call_link().is_some() {
            continue;
        }
        if recipients.by_chat.contains_key(&chat.id) {
            kept.push(chat);
            continue;
        }
        let destination = match chat.kind {
            ConversationKind::NoteToSelf => continue,
            ConversationKind::Direct => match contact(db, chat).await {
                Some(contact) => proto::recipient::Destination::Contact(contact),
                None => {
                    stats.skip("person without an ACI");
                    continue;
                }
            },
            ConversationKind::Group => match group(db, chat, &me).await {
                Some(group) => proto::recipient::Destination::Group(group),
                None => {
                    stats.skip("group without a master key");
                    continue;
                }
            },
        };
        let id = recipients.take_id();
        recipients.by_chat.insert(chat.id.clone(), id);
        let recipient = proto::Recipient { id, destination: Some(destination), ..Default::default() };
        put(&mut out, &frame(proto::frame::Item::Recipient(recipient)))?;
        stats.recipients += 1;
        kept.push(chat);
    }
    // Group members without a chat of their own: before the groups' chats
    // name them (only groups' snapshots do, and those carry ACIs).
    for frame in std::mem::take(&mut recipients.pending) {
        put(&mut out, &frame)?;
        stats.recipients += 1;
    }

    // Installed sticker packs.
    for pack in db.sticker_pack_rows().await.unwrap_or_default().into_iter().filter(|p| p.installed) {
        let (Ok(id), Ok(key)) = (hex::decode(&pack.id), hex::decode(&pack.key)) else { continue };
        let pack = proto::StickerPack { packId: id, packKey: key, ..Default::default() };
        put(&mut out, &frame(proto::frame::Item::InstalledStickerPack(pack)))?;
    }

    // Chats, with the same ids as their recipients (as Desktop's).
    let pinned = db.pinned_ids().await.unwrap_or_default();
    for chat in &kept {
        let id = recipients.by_chat[&chat.id];
        let mut c = proto::Chat::new();
        c.id = id;
        c.recipientId = id;
        c.archived = chat.archived;
        c.pinnedOrder = pinned.iter().position(|p| *p == chat.id).map(|ix| ix as u32 + 1);
        if chat.expire_timer > 0 {
            c.expirationTimerMs = Some(u64::from(chat.expire_timer) * 1000);
        }
        c.expireTimerVersion = chat.expire_timer_version;
        if chat.muted_until > 0 {
            c.muteUntilMs = Some(chat.muted_until as u64);
        }
        c.markedUnread = chat.marked_unread;
        c.dontNotifyForMentionsIfMuted = chat.dont_notify_mentions;
        c.notifyForCallsIfMuted = chat.notify.calls;
        if chat.kind == ConversationKind::Group {
            c.notifyForMentionsIfMuted = chat.notify.mentions;
            c.notifyForRepliesIfMuted = chat.notify.replies;
        }
        c.showUnreadReminders = chat.notify.unread_reminders;
        put(&mut out, &frame(proto::frame::Item::Chat(c)))?;
        stats.chats += 1;
    }

    // Chat folders; the backup must have exactly one "All chats".
    let folders = db.chat_folders().await.unwrap_or_default();
    let mut has_all = false;
    for folder in folders.iter().filter(|f| f.deleted_at == 0) {
        let Ok(uuid) = uuid::Uuid::parse_str(&folder.id) else { continue };
        let folder_type = match folder.folder_type {
            langis_store::ChatFolderType::All if has_all => continue,
            langis_store::ChatFolderType::All => {
                has_all = true;
                proto::chat_folder::FolderType::ALL
            }
            langis_store::ChatFolderType::Custom => proto::chat_folder::FolderType::CUSTOM,
            // Desktop drops a folder of a type it doesn't know.
            _ => continue,
        };
        let ids = |chats: &[ConversationId]| chats.iter().filter_map(|c| recipients.by_chat.get(c).copied()).collect::<Vec<_>>();
        let f = proto::ChatFolder {
            id: uuid.into_bytes().to_vec(),
            name: folder.name.clone(),
            folderType: folder_type.into(),
            showOnlyUnread: folder.show_only_unread,
            showMutedChats: folder.show_muted_chats,
            includeAllIndividualChats: folder.include_all_individual_chats,
            includeAllGroupChats: folder.include_all_group_chats,
            includedRecipientIds: ids(&folder.included),
            excludedRecipientIds: ids(&folder.excluded),
            ..Default::default()
        };
        put(&mut out, &frame(proto::frame::Item::ChatFolder(f)))?;
    }
    if !has_all {
        let all = proto::ChatFolder {
            id: uuid::Uuid::new_v4().into_bytes().to_vec(),
            folderType: proto::chat_folder::FolderType::ALL.into(),
            showMutedChats: true,
            includeAllIndividualChats: true,
            includeAllGroupChats: true,
            ..Default::default()
        };
        put(&mut out, &frame(proto::frame::Item::ChatFolder(all)))?;
    }

    // Messages, a chat at a time, oldest first.
    let mut ctx = ItemContext { db, now, file_secret, files: &mut files, seen_files: HashMap::new() };
    for chat in &kept {
        let chat_id = recipients.by_chat[&chat.id];
        let mut after = None;
        loop {
            let page = db.messages_in_order(&chat.id, after, 500).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for message in &page {
                match chat_item(&mut ctx, &mut recipients, &mut stats, chat, chat_id, message).await? {
                    Some(item) => {
                        for frame in std::mem::take(&mut recipients.pending) {
                            put(&mut out, &frame)?;
                            stats.recipients += 1;
                        }
                        put(&mut out, &frame(proto::frame::Item::ChatItem(item)))?;
                        stats.messages += 1;
                    }
                    None => recipients.pending.clear(),
                }
            }
        }
    }
    stats.files = files.len();
    Ok(Frames { plain: out, files, stats })
}

/// A person's contact record (Desktop's `#toRecipient` for a direct chat).
async fn contact(db: &AppDb, chat: &Conversation) -> Option<proto::Contact> {
    let sid = chat.id.as_direct()?;
    let mut contact = proto::Contact::new();
    contact.aci = Some(uuid_bytes(sid)?);
    contact.e164 = chat.phone.as_deref().and_then(e164);
    contact.blocked = chat.blocked;
    if chat.blocked {
        contact.blockedAtTimestamp = chat.blocked_at.unwrap_or(0).max(0) as u64;
    }
    contact.registration = Some(match chat.unregistered_at {
        Some(at) => proto::contact::Registration::NotRegistered(proto::contact::NotRegistered {
            unregisteredTimestamp: at.max(0) as u64,
            ..Default::default()
        }),
        None => proto::contact::Registration::Registered(Default::default()),
    });
    if chat.removal_stage.is_some() {
        contact.visibility = proto::contact::Visibility::HIDDEN.into();
    }
    contact.profileSharing = db.profile_sharing(&chat.id).await.unwrap_or(false);
    let profile = db.person_profile(sid).await.ok().flatten();
    if let Some(profile) = &profile {
        contact.profileGivenName = Some(profile.given_name.clone()).filter(|n| !n.is_empty());
        contact.profileFamilyName = Some(profile.family_name.clone()).filter(|n| !n.is_empty());
        contact.username = profile.username.clone().filter(|u| usernames::Username::new(u).is_ok());
    }
    if let Ok(Some(nickname)) = db.contact_nickname(sid).await {
        if !nickname.given.is_empty() || !nickname.family.is_empty() {
            contact.nickname =
                Some(proto::contact::Name { given: nickname.given.clone(), family: nickname.family.clone(), ..Default::default() }).into();
        }
        contact.note = nickname.note;
    }
    // The name shown, when it's the address book's or nothing else has it.
    let title = chat.title.trim();
    let shown_otherwise = profile.as_ref().and_then(|p| p.profile_name()).is_some_and(|n| n == title)
        || chat.phone.as_deref() == Some(title)
        || title.is_empty()
        || title == langis_store::UNKNOWN_NAME;
    if chat.system_contact || !shown_otherwise && contact.profileGivenName.is_none() && contact.nickname.is_none() {
        if chat.system_contact {
            contact.systemGivenName = title.to_owned();
        } else if !shown_otherwise {
            contact.profileGivenName = Some(title.to_owned());
        }
    }
    Some(contact)
}

/// A group's record and snapshot (Desktop's `#toRecipient` for a group):
/// its title, description, timer, access, members and admins.
async fn group(db: &AppDb, chat: &Conversation, me: &str) -> Option<proto::Group> {
    use proto::group::{AccessControl, GroupAttributeBlob, Member, access_control::AccessRequired, group_attribute_blob::Content, member::Role};
    let key = chat.id.as_group().filter(|k| k.len() == 32)?;
    let blob = |content: Content| Some(GroupAttributeBlob { content: Some(content), ..Default::default() }).into();
    let mut snapshot = proto::group::GroupSnapshot::new();
    if !chat.title.is_empty() {
        snapshot.title = blob(Content::Title(chat.title.clone()));
    }
    if let Some(description) = chat.description.clone().filter(|d| !d.is_empty()) {
        snapshot.description = blob(Content::DescriptionText(description));
    }
    if chat.expire_timer > 0 {
        snapshot.disappearingMessagesTimer = blob(Content::DisappearingMessagesDuration(chat.expire_timer));
    }
    let state = &chat.group_state;
    let access = |admins: bool| if admins { AccessRequired::ADMINISTRATOR } else { AccessRequired::MEMBER };
    snapshot.accessControl = Some(AccessControl {
        attributes: access(state.info_admins_only).into(),
        members: access(state.members_admins_only).into(),
        addFromInviteLink: match &state.link {
            Some(link) if link.approval => AccessRequired::ADMINISTRATOR,
            Some(_) => AccessRequired::ANY,
            None => AccessRequired::UNSATISFIABLE,
        }
        .into(),
        memberLabel: access(state.labels_admins_only).into(),
        ..Default::default()
    })
    .into();
    snapshot.announcementsOnly = state.announcements_only;
    snapshot.terminated = chat.ended;
    let mut members: Vec<String> = chat.members.clone();
    // We're a member unless we left; the store lists the others.
    if !chat.left && !members.iter().any(|m| m == me) && matches!(state.membership, langis_store::Membership::Member) {
        members.push(me.to_owned());
    }
    for member in &members {
        let Some(user_id) = uuid_bytes(member) else { continue };
        let role = if chat.admins.contains(member) { Role::ADMINISTRATOR } else { Role::DEFAULT };
        let label = chat.member_labels.get(member);
        snapshot.members.push(Member {
            userId: user_id,
            role: role.into(),
            labelString: label.map(|l| l.text.clone()).unwrap_or_default(),
            labelEmoji: label.filter(|l| !l.text.is_empty()).and_then(|l| l.emoji.clone()).unwrap_or_default(),
            ..Default::default()
        });
    }
    for banned in &state.banned {
        let Some(user_id) = langis_store_service_id_bytes(banned) else { continue };
        snapshot.membersBanned.push(proto::group::MemberBanned { userId: user_id, ..Default::default() });
    }
    let mut group = proto::Group::new();
    group.masterKey = key;
    group.whitelisted = db.profile_sharing(&chat.id).await.unwrap_or(false);
    group.blocked = chat.blocked;
    if chat.blocked {
        group.blockedAtTimestamp = chat.blocked_at.unwrap_or(0).max(0) as u64;
    }
    let mode = db.story_send_mode(&chat.id).await.unwrap_or_default();
    group.storySendMode = protobuf::EnumOrUnknown::from_i32(mode.to_wire() as i32);
    group.snapshot = Some(snapshot).into();
    Some(group)
}

/// A service id (ACI, or `PNI:` and a PNI) as the backup's binary form.
fn langis_store_service_id_bytes(service_id: &str) -> Option<Vec<u8>> {
    presage::libsignal_service::protocol::ServiceId::parse_from_service_id_string(service_id).map(|s| s.service_id_binary())
}

struct ItemContext<'a> {
    db: &'a AppDb,
    now: i64,
    file_secret: &'a [u8; 32],
    files: &'a mut Vec<FileJob>,
    /// Files already hashed, by path: (plaintext hash, size).
    seen_files: HashMap<String, ([u8; 32], u64)>,
}

/// Body ranges for a shown text: each "@Name" back to a placeholder (one
/// UTF-16 unit naming the person) and the formatting over the text as
/// sent. Desktop's `#toBodyRanges`.
fn text_of(body: &str, mentions: &[langis_store::MentionSpan], styles: &[langis_store::StyleSpan]) -> proto::Text {
    use proto::body_range::{AssociatedValue, Style};
    let mut spans: Vec<&langis_store::MentionSpan> =
        mentions.iter().filter(|m| body.get(m.start..m.start + m.len).is_some()).collect();
    spans.sort_by_key(|m| m.start);
    // Where each byte of `body` lands in the text as sent, in UTF-16 units.
    let mut at = vec![0u32; body.len() + 1];
    let mut wire = String::with_capacity(body.len());
    let mut ranges = Vec::new();
    let mut utf16 = 0u32;
    let mut byte = 0;
    let mut spans_iter = spans.iter().peekable();
    while byte < body.len() {
        if let Some(span) = spans_iter.peek().filter(|s| s.start == byte) {
            let end = span.start + span.len;
            for b in byte..end {
                at[b] = utf16;
            }
            if let Some(aci) = uuid_bytes(&span.aci) {
                ranges.push(proto::BodyRange {
                    start: utf16,
                    length: 1,
                    associatedValue: Some(AssociatedValue::MentionAci(aci)),
                    ..Default::default()
                });
                wire.push(crate::mapping::MENTION_PLACEHOLDER);
                utf16 += 1;
            } else {
                wire.push_str(&body[byte..end]);
                utf16 += body[byte..end].encode_utf16().count() as u32;
            }
            byte = end;
            spans_iter.next();
            continue;
        }
        // Skip spans that overlap one already written.
        if spans_iter.peek().is_some_and(|s| s.start < byte) {
            spans_iter.next();
            continue;
        }
        let c = body[byte..].chars().next().unwrap_or(' ');
        for b in byte..byte + c.len_utf8() {
            at[b] = utf16;
        }
        wire.push(c);
        utf16 += c.len_utf16() as u32;
        byte += c.len_utf8();
    }
    at[body.len()] = utf16;
    for style in styles {
        let (Some(start), Some(end)) = (at.get(style.start), at.get(style.start + style.len)) else { continue };
        if end <= start {
            continue;
        }
        let style_value = match style.style {
            langis_store::TextStyle::Bold => Style::BOLD,
            langis_store::TextStyle::Italic => Style::ITALIC,
            langis_store::TextStyle::Spoiler => Style::SPOILER,
            langis_store::TextStyle::Strikethrough => Style::STRIKETHROUGH,
            langis_store::TextStyle::Monospace => Style::MONOSPACE,
        };
        ranges.push(proto::BodyRange {
            start: *start,
            length: end - start,
            associatedValue: Some(AssociatedValue::Style(style_value.into())),
            ..Default::default()
        });
    }
    if wire.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES;
        while !wire.is_char_boundary(end) {
            end -= 1;
        }
        wire.truncate(end);
    }
    proto::Text { body: wire, bodyRanges: ranges, ..Default::default() }
}

/// A file's pointer, and the job copying it into the backup when this
/// device has it (Desktop's `getFilePointerForAttachment` for
/// `local-encrypted`). `None` when there's neither a file nor anywhere to
/// download it from: the backup format has no pointer for that.
async fn file_pointer(ctx: &mut ItemContext<'_>, meta: &langis_store::AttachmentMeta) -> Option<proto::FilePointer> {
    use proto::file_pointer::{LocatorInfo, locator_info::IntegrityCheck};
    let mut pointer = proto::FilePointer::new();
    pointer.contentType = Some(meta.content_type.clone()).filter(|t| !t.is_empty());
    pointer.fileName = meta.file_name.clone();
    pointer.width = meta.width;
    pointer.height = meta.height;
    pointer.blurHash = meta.blurhash.clone();
    let remote = meta.remote.as_ref();
    let remote_key = remote.and_then(|r| hex::decode(&r.key).ok()).filter(|k| k.len() == 64);
    let transit = remote.and_then(|r| r.cdn_key.clone().map(|key| (key, r.cdn))).filter(|_| remote_key.is_some());
    let mut locator = LocatorInfo::new();
    let local = match meta.local_path.as_deref() {
        Some(path) => local_file(ctx, path).await,
        None => None,
    };
    if let Some((hash, size)) = local {
        let local_key = derive_key(ctx.file_secret, b"localKey", &hash);
        // Desktop gives a downloaded file without a key a new one
        // (`maybeFixupAttachment`), and keeps transit details only with
        // the key they belong to.
        locator.key = remote_key.clone().unwrap_or_else(|| derive_key(ctx.file_secret, b"key", &hash).to_vec());
        locator.integrityCheck = Some(IntegrityCheck::PlaintextHash(hash.to_vec()));
        locator.size = u32::try_from(size).unwrap_or(u32::MAX);
        if let Some((key, cdn)) = transit {
            locator.transitCdnKey = Some(key);
            locator.transitCdnNumber = Some(cdn);
        }
        locator.mediaTierCdnNumber = remote.filter(|r| r.plaintext_hash.is_some()).and_then(|r| r.media_tier_cdn);
        locator.localKey = Some(local_key.to_vec());
        let media_name = media_name(&hash, &local_key);
        if !ctx.files.iter().any(|f| f.media_name == media_name) {
            ctx.files.push(FileJob { media_name, source: PathBuf::from(meta.local_path.as_deref()?), local_key, size });
        }
    } else {
        let remote = remote?;
        let key = remote_key?;
        if let Some(hash) = remote.plaintext_hash.as_deref().and_then(|h| hex::decode(h).ok()).filter(|h| h.len() == 32) {
            locator.integrityCheck = Some(IntegrityCheck::PlaintextHash(hash));
            locator.mediaTierCdnNumber = remote.media_tier_cdn;
        } else if let (Some(digest), Some(_)) = (remote.digest.as_deref().and_then(|d| hex::decode(d).ok()), &transit) {
            locator.integrityCheck = Some(IntegrityCheck::EncryptedDigest(digest));
        } else {
            return None;
        }
        locator.key = key;
        locator.size = u32::try_from(meta.size).unwrap_or(u32::MAX);
        if let Some((key, cdn)) = transit {
            locator.transitCdnKey = Some(key);
            locator.transitCdnNumber = Some(cdn);
        }
    }
    pointer.locatorInfo = Some(locator).into();
    Some(pointer)
}

/// A downloaded file's plaintext hash and size, read once per backup.
async fn local_file(ctx: &mut ItemContext<'_>, path: &str) -> Option<([u8; 32], u64)> {
    if let Some(found) = ctx.seen_files.get(path) {
        return Some(*found);
    }
    let owned = path.to_owned();
    let hashed = tokio::task::spawn_blocking(move || -> std::io::Result<([u8; 32], u64)> {
        use sha2::Digest as _;
        use std::io::Read as _;
        let mut file = std::fs::File::open(&owned)?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut size = 0u64;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        Ok((hasher.finalize().into(), size))
    })
    .await
    .ok()?
    .ok()?;
    ctx.seen_files.insert(path.to_owned(), hashed);
    Some(hashed)
}

/// 64 bytes for a file from the backup's file secret: its `localKey`, or
/// its key when it never had one. The same file always gets the same, so
/// a file already in `files/` isn't copied again (Desktop's `localKey` is
/// kept per attachment for the same reason).
fn derive_key(secret: &[u8; 32], label: &[u8], plaintext_hash: &[u8; 32]) -> [u8; 64] {
    use hmac::{KeyInit as _, Mac as _};
    let mut mac = hmac::Hmac::<sha2::Sha512>::new_from_slice(secret).expect("any key length");
    mac.update(label);
    mac.update(plaintext_hash);
    mac.finalize().into_bytes().into()
}

/// Desktop's `getLocalBackupFileName`: hex(sha256(plaintextHash || localKey)).
fn media_name(plaintext_hash: &[u8; 32], local_key: &[u8; 64]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(plaintext_hash);
    hasher.update(local_key);
    hex::encode(hasher.finalize())
}

/// One message or notice as a chat item; `None` (counted in `stats`) for
/// what isn't backed up.
async fn chat_item(
    ctx: &mut ItemContext<'_>,
    recipients: &mut Recipients,
    stats: &mut Stats,
    chat: &Conversation,
    chat_id: u64,
    message: &Message,
) -> anyhow::Result<Option<proto::ChatItem>> {
    use proto::chat_item::{DirectionalDetails, IncomingMessageDetails, Item, OutgoingMessageDetails};
    let mut item = proto::ChatItem::new();
    item.chatId = chat_id;
    item.dateSent = message.sent_at.max(0) as u64;
    if let Some(notice) = &message.notice {
        let Some((author, update)) = notice_update(recipients, chat, notice, message.sent_at) else {
            stats.skip(notice_kind(notice));
            return Ok(None);
        };
        item.authorId = author;
        item.directionalDetails = Some(DirectionalDetails::Directionless(Default::default()));
        item.item = Some(Item::UpdateMessage(proto::ChatUpdateMessage { update: Some(update), ..Default::default() }));
        return Ok(Some(item));
    }
    // Disappearing messages: none that go within a day (Desktop's
    // `exclude-short-lived`).
    if message.expires_in > 0 {
        let lifetime = i64::from(message.expires_in) * 1000;
        if lifetime <= MINIMUM_LIFETIME_MS {
            stats.skip("disappears within a day");
            return Ok(None);
        }
        let start = message.expire_start.or_else(|| {
            // Read or sent: its timer has started (the validator insists).
            (message.direction == Direction::Outgoing || message.is_read).then_some(message.received_at)
        });
        if let Some(start) = start {
            if start + lifetime <= ctx.now + MINIMUM_LIFETIME_MS {
                stats.skip("disappears within a day");
                return Ok(None);
            }
            item.expireStartDate = Some(start.max(0) as u64);
        }
        item.expiresInMs = Some(lifetime as u64);
    }
    match message.direction {
        Direction::Incoming => {
            let Some(author) = recipients.person(message.author.as_deref()).filter(|a| *a != recipients.self_id) else {
                stats.skip("incoming without an author");
                return Ok(None);
            };
            item.authorId = author;
            item.directionalDetails = Some(DirectionalDetails::Incoming(IncomingMessageDetails {
                dateReceived: message.received_at.max(0) as u64,
                read: message.is_read,
                sealedSender: !message.sealed_sender.is_empty(),
                ..Default::default()
            }));
        }
        Direction::Outgoing => {
            item.authorId = recipients.self_id;
            let statuses = send_statuses(ctx.db, recipients, chat, message).await;
            item.directionalDetails = Some(DirectionalDetails::Outgoing(OutgoingMessageDetails {
                sendStatus: statuses,
                dateReceived: message.received_at.max(0) as u64,
                ..Default::default()
            }));
        }
    }
    let reactions = reactions(recipients, &message.reactions);
    let body = item_body(ctx, recipients, stats, message, reactions).await;
    let Some(body) = body else { return Ok(None) };
    item.item = Some(body);
    // An edit: dated by its latest edit, its earlier versions oldest first
    // (Desktop's `#toChatItemRevisions`).
    if message.edited && !message.deleted && matches!(item.item, Some(Item::StandardMessage(_))) {
        let history = ctx.db.edit_history(message.id).await.unwrap_or_default();
        if let Some(latest) = ctx.db.latest_edit(message.id).await.ok().flatten() {
            item.dateSent = latest.max(message.sent_at).max(0) as u64;
        }
        for revision in history.iter().filter(|r| (r.sent_at.max(0) as u64) < item.dateSent) {
            let mut earlier = proto::ChatItem::new();
            earlier.chatId = item.chatId;
            earlier.authorId = item.authorId;
            earlier.dateSent = revision.sent_at.max(0) as u64;
            earlier.expireStartDate = item.expireStartDate;
            earlier.expiresInMs = item.expiresInMs;
            earlier.directionalDetails = item.directionalDetails.clone();
            let standard = proto::StandardMessage {
                text: Some(text_of(&revision.body, &revision.mentions, &revision.styles)).into(),
                ..Default::default()
            };
            earlier.item = Some(Item::StandardMessage(standard));
            item.revisions.push(earlier);
        }
    }
    if let Some(pin) = ctx.db.pinned_messages(&message.conversation_id).await.ok().and_then(|pins| {
        pins.iter().any(|p| p.id == message.id).then_some(())
    }) {
        let _ = pin;
        let mut details = proto::chat_item::PinDetails::new();
        details.pinnedAtTimestamp = item.dateSent;
        details.pinExpiry = Some(proto::chat_item::pin_details::PinExpiry::PinNeverExpires(true));
        item.pinDetails = Some(details).into();
    }
    Ok(Some(item))
}

fn reactions(recipients: &mut Recipients, reactions: &[langis_store::Reaction]) -> Vec<proto::Reaction> {
    reactions
        .iter()
        .enumerate()
        .filter_map(|(ix, r)| {
            Some(proto::Reaction {
                emoji: r.emoji.clone(),
                authorId: recipients.person(r.author.as_deref())?,
                sentTimestamp: r.sent_at.max(0) as u64,
                sortOrder: ix as u64 + 1,
                ..Default::default()
            })
        })
        .collect()
}

/// Each recipient's status for our message (Desktop's `#toSendStatus`):
/// what's stored per recipient, else the message's own status for each.
async fn send_statuses(db: &AppDb, recipients: &mut Recipients, chat: &Conversation, message: &Message) -> Vec<proto::SendStatus> {
    use proto::send_status::{DeliveryStatus as S, Failed, failed::FailureReason};
    if chat.kind == ConversationKind::NoteToSelf {
        return Vec::new();
    }
    let status_of = |status: langis_store::SendStatus, error: Option<&str>| match status {
        langis_store::SendStatus::Pending => S::Pending(Default::default()),
        langis_store::SendStatus::Sent => S::Sent(Default::default()),
        langis_store::SendStatus::Delivered => S::Delivered(Default::default()),
        langis_store::SendStatus::Read => S::Read(Default::default()),
        langis_store::SendStatus::Viewed => S::Viewed(Default::default()),
        langis_store::SendStatus::Skipped => S::Skipped(Default::default()),
        langis_store::SendStatus::Failed => S::Failed(Failed {
            reason: if error == Some(langis_store::SEND_ERROR_IDENTITY) { FailureReason::IDENTITY_KEY_MISMATCH } else { FailureReason::UNKNOWN }
                .into(),
            ..Default::default()
        }),
    };
    let stored = db.send_states(message.id).await.unwrap_or_default();
    if !stored.is_empty() {
        return stored
            .iter()
            .filter_map(|s| {
                Some(proto::SendStatus {
                    recipientId: recipients.person(Some(&s.recipient)).filter(|id| *id != recipients.self_id)?,
                    timestamp: s.updated_at.unwrap_or(message.sent_at).max(0) as u64,
                    deliveryStatus: Some(status_of(s.status, s.error.as_deref())),
                    ..Default::default()
                })
            })
            .collect();
    }
    let status = match message.status {
        DeliveryStatus::Sending | DeliveryStatus::Paused => S::Pending(Default::default()),
        DeliveryStatus::Sent | DeliveryStatus::PartialSent => S::Sent(Default::default()),
        DeliveryStatus::Delivered => S::Delivered(Default::default()),
        DeliveryStatus::Read => S::Read(Default::default()),
        DeliveryStatus::Viewed => S::Viewed(Default::default()),
        DeliveryStatus::Failed => S::Failed(Default::default()),
    };
    let to: Vec<String> = match chat.kind {
        ConversationKind::Direct => chat.id.as_direct().map(str::to_owned).into_iter().collect(),
        _ => chat.members.iter().filter(|m| *m != recipients.me.as_str()).cloned().collect(),
    };
    to.iter()
        .filter_map(|sid| {
            Some(proto::SendStatus {
                recipientId: recipients.person(Some(sid)).filter(|id| *id != recipients.self_id)?,
                timestamp: message.sent_at.max(0) as u64,
                deliveryStatus: Some(status.clone()),
                ..Default::default()
            })
        })
        .collect()
}

/// What a message is, as a chat item's `item`.
async fn item_body(
    ctx: &mut ItemContext<'_>,
    recipients: &mut Recipients,
    stats: &mut Stats,
    message: &Message,
    reactions: Vec<proto::Reaction>,
) -> Option<proto::chat_item::Item> {
    use proto::chat_item::Item;
    if message.deleted {
        return Some(Item::RemoteDeletedMessage(Default::default()));
    }
    if message.view_once.is_some() {
        // Desktop's local backups never carry a view-once message's file.
        return Some(Item::ViewOnceMessage(proto::ViewOnceMessage { reactions, ..Default::default() }));
    }
    if message.content.is_some() {
        stats.skip("shared contact, payment or gift badge");
        return None;
    }
    if let Some(poll) = &message.poll {
        let options = poll
            .options
            .iter()
            .enumerate()
            .map(|(ix, option)| {
                let votes = poll
                    .votes
                    .iter()
                    .filter(|v| v.options.contains(&(ix as u32)))
                    .filter_map(|v| {
                        Some(proto::poll::poll_option::PollVote {
                            voterId: recipients.person(v.author.as_deref())?,
                            voteCount: v.vote_count,
                            ..Default::default()
                        })
                    })
                    .collect();
                proto::poll::PollOption { option: option.clone(), votes, ..Default::default() }
            })
            .collect();
        return Some(Item::Poll(proto::Poll {
            question: poll.question.clone(),
            allowMultiple: poll.allow_multiple,
            options,
            hasEnded: poll.ended,
            reactions,
            ..Default::default()
        }));
    }
    if let Some(sticker) = message.attachments.first().filter(|a| a.sticker) {
        let Some(pack) = &sticker.sticker_pack else {
            stats.skip("sticker without its pack");
            return None;
        };
        let (Ok(id), Ok(key)) = (hex::decode(&pack.id), hex::decode(&pack.key)) else {
            stats.skip("sticker without its pack");
            return None;
        };
        let Some(data) = file_pointer(ctx, sticker).await else {
            stats.skip("no file or locator");
            return None;
        };
        let sticker = proto::Sticker {
            packId: id,
            packKey: key,
            stickerId: pack.sticker_id.unwrap_or(0),
            data: Some(data).into(),
            ..Default::default()
        };
        return Some(Item::StickerMessage(proto::StickerMessage { sticker: Some(sticker).into(), reactions, ..Default::default() }));
    }
    let mut standard = proto::StandardMessage::new();
    if !message.body.is_empty() {
        standard.text = Some(text_of(&message.body, &message.mentions, &message.styles)).into();
    }
    for attachment in &message.attachments {
        let Some(pointer) = file_pointer(ctx, attachment).await else {
            stats.skip("no file or locator");
            continue;
        };
        use proto::message_attachment::Flag;
        let flag = if attachment.voice {
            Flag::VOICE_MESSAGE
        } else if attachment.gif {
            Flag::GIF
        } else {
            Flag::NONE
        };
        standard.attachments.push(proto::MessageAttachment {
            pointer: Some(pointer).into(),
            flag: flag.into(),
            wasDownloaded: attachment.local_path.is_some(),
            ..Default::default()
        });
    }
    // The whole text, when it's still to download (Desktop backs up a long
    // text's pointer only then; downloaded, it's all in the body).
    if let Some(long) = &message.long_text
        && !message.body.is_empty()
    {
        let meta = langis_store::AttachmentMeta {
            content_type: "text/x-signal-plain".into(),
            file_name: None,
            size: long.size,
            width: None,
            height: None,
            local_path: None,
            sticker: false,
            gif: false,
            voice: false,
            remote: Some(long.remote.clone()),
            thumbnail_path: None,
            sticker_pack: None,
            transcode: false,
            blurhash: None,
        };
        standard.longText = file_pointer(ctx, &meta).await.into();
    }
    if let Some(preview) = &message.link_preview {
        let image = match &preview.image {
            Some(image) => file_pointer(ctx, image).await,
            None => None,
        };
        standard.linkPreview.push(proto::LinkPreview {
            url: preview.url.clone(),
            title: preview.title.clone(),
            description: preview.description.clone(),
            date: preview.date.map(|d| d.max(0) as u64),
            image: image.into(),
            ..Default::default()
        });
    }
    if let Some(quote) = &message.quote {
        let mut q = proto::Quote::new();
        q.targetSentTimestamp = Some(quote.sent_at.max(0) as u64);
        q.authorId = recipients.person(quote.author.as_deref()).unwrap_or(recipients.self_id);
        if !quote.text.is_empty() {
            q.text = Some(proto::Text { body: quote.text.clone(), ..Default::default() }).into();
        }
        let view_once = quote.attachment.as_ref().is_some_and(|a| a.view_once);
        q.type_ = if view_once { proto::quote::Type::VIEW_ONCE } else { proto::quote::Type::NORMAL }.into();
        if let Some(a) = quote.attachment.as_ref().filter(|_| !view_once) {
            q.attachments.push(proto::quote::QuotedAttachment {
                contentType: Some(a.content_type.clone()).filter(|t| !t.is_empty()),
                fileName: a.file_name.clone(),
                ..Default::default()
            });
        }
        standard.quote = Some(q).into();
    }
    if standard.text.is_none() && standard.attachments.is_empty() {
        stats.skip("nothing left to show");
        return None;
    }
    standard.reactions = reactions;
    Some(Item::StandardMessage(standard))
}

fn notice_kind(notice: &Notice) -> &'static str {
    match notice {
        Notice::GroupCall { .. } => "group call",
        Notice::ContactRemoved => "contact removed notice",
        Notice::Call { .. } => "call outside a 1:1 chat",
        _ => "notice naming no one known",
    }
}

/// A notice as an update and its author (Desktop's `toChatItemUpdate`, and
/// the reverse of `history.rs`'s `update_notices`). `None` for notices the
/// backup format has no update for.
fn notice_update(recipients: &mut Recipients, chat: &Conversation, notice: &Notice, sent_at: i64) -> Option<(u64, proto::chat_update_message::Update)> {
    use proto::chat_update_message::Update;
    use proto::simple_chat_update::Type;
    let me = recipients.self_id;
    let simple = |kind: Type| Update::SimpleUpdate(proto::SimpleChatUpdate { type_: kind.into(), ..Default::default() });
    // The person a 1:1 chat is with.
    let theirs = chat.id.as_direct().filter(|_| chat.kind == ConversationKind::Direct).map(str::to_owned);
    Some(match notice {
        Notice::Timer { by, seconds } if chat.kind == ConversationKind::Group => {
            let change = proto::GroupExpirationTimerUpdate {
                expiresInMs: u64::from(*seconds) * 1000,
                updaterAci: aci_or_me(recipients, by.as_deref()),
                ..Default::default()
            };
            (recipients.person(by.as_deref())?, group_change(vec![proto::group_change_chat_update::update::Update::GroupExpirationTimerUpdate(change)]))
        }
        Notice::Timer { by, seconds } => (
            recipients.person(by.as_deref())?,
            Update::ExpirationTimerChange(proto::ExpirationTimerChatUpdate { expiresInMs: u64::from(*seconds) * 1000, ..Default::default() }),
        ),
        Notice::Pinned { by, author, sent_at } => (
            recipients.person(by.as_deref())?,
            Update::PinMessage(proto::PinMessageUpdate {
                targetSentTimestamp: sent_at.max(&0).to_owned() as u64,
                authorId: recipients.person(author.as_deref())?,
                ..Default::default()
            }),
        ),
        Notice::PollEnded { by, question, sent_at } => (
            recipients.person(by.as_deref())?,
            Update::PollTerminate(proto::PollTerminateUpdate {
                targetSentTimestamp: (*sent_at).max(0) as u64,
                question: question.clone(),
                ..Default::default()
            }),
        ),
        Notice::SafetyNumberChanged { who } => (recipients.person(Some(who))?, simple(Type::IDENTITY_UPDATE)),
        Notice::Verified { who, verified, .. } => {
            (recipients.person(Some(who))?, simple(if *verified { Type::IDENTITY_VERIFIED } else { Type::IDENTITY_DEFAULT }))
        }
        Notice::NumberChanged { who } => (recipients.person(Some(who))?, simple(Type::CHANGE_NUMBER)),
        Notice::SessionRefreshed => (recipients.person(theirs.as_deref()).unwrap_or(me), simple(Type::CHAT_SESSION_REFRESH)),
        Notice::SessionEnded => (recipients.person(theirs.as_deref()).unwrap_or(me), simple(Type::END_SESSION)),
        Notice::DeliveryIssue { who } => (recipients.person(Some(who))?, simple(Type::BAD_DECRYPT)),
        Notice::JoinedSignal { who } => (recipients.person(Some(who))?, simple(Type::JOINED_SIGNAL)),
        Notice::UnsupportedMessage { who } => (recipients.person(who.as_deref())?, simple(Type::UNSUPPORTED_PROTOCOL_MESSAGE)),
        Notice::MessageRequest { event, .. } => {
            use langis_store::MessageRequestEvent as E;
            let kind = match event {
                E::Accept => Type::MESSAGE_REQUEST_ACCEPTED,
                E::Block => Type::BLOCKED,
                E::Unblock => Type::UNBLOCKED,
                E::Spam => Type::REPORTED_SPAM,
                #[allow(unreachable_patterns)]
                _ => return None,
            };
            (me, simple(kind))
        }
        Notice::ProfileNameChanged { who, old_name, new_name } => (
            recipients.person(Some(who))?,
            Update::ProfileChange(proto::ProfileChangeChatUpdate {
                previousName: old_name.clone(),
                newName: new_name.clone(),
                ..Default::default()
            }),
        ),
        Notice::Merged { who, phone } => (
            recipients.person(Some(who))?,
            Update::ThreadMerge(proto::ThreadMergeChatUpdate {
                previousE164: phone.as_deref().and_then(e164).unwrap_or(0),
                ..Default::default()
            }),
        ),
        Notice::NumberIsTheirs { who, phone } => (
            recipients.person(Some(who))?,
            Update::SessionSwitchover(proto::SessionSwitchoverChatUpdate { e164: e164(phone)?, ..Default::default() }),
        ),
        Notice::StartedWith { who, previous } => {
            use proto::learned_profile_chat_update::PreviousName;
            let previous = match e164(previous) {
                Some(number) => PreviousName::E164(number),
                None if usernames::Username::new(previous).is_ok() => PreviousName::Username(previous.clone()),
                None => PreviousName::SharedName(previous.clone()),
            };
            (
                recipients.person(Some(who))?,
                Update::LearnedProfileChange(proto::LearnedProfileChatUpdate { previousName: Some(previous), ..Default::default() }),
            )
        }
        Notice::Call { incoming, video, outcome, call_id, .. } if chat.kind == ConversationKind::Direct => {
            use langis_store::CallOutcome;
            use proto::individual_call::{Direction as D, State, Type as T};
            let state = match outcome {
                CallOutcome::Accepted => State::ACCEPTED,
                CallOutcome::Missed => State::MISSED,
                CallOutcome::Declined | CallOutcome::Unanswered => State::NOT_ACCEPTED,
            };
            let call = proto::IndividualCall {
                callId: call_id.map(|id| id as u64),
                type_: if *video { T::VIDEO_CALL } else { T::AUDIO_CALL }.into(),
                direction: if *incoming { D::INCOMING } else { D::OUTGOING }.into(),
                state: state.into(),
                read: true,
                ..Default::default()
            };
            let author = if *incoming { recipients.person(theirs.as_deref())? } else { me };
            (author, Update::IndividualCall(call))
        }
        Notice::Group { by, change } => {
            let author = match by.as_deref() {
                Some(langis_store::UNKNOWN_ACTOR) | None => me,
                Some(sid) => recipients.person(Some(sid)).unwrap_or(me),
            };
            (author, group_change(group_updates(recipients, by.as_deref(), change)?))
        }
        // Desktop's `toGroupCall`: who started it, its state, when.
        Notice::GroupCall { by, call_id, status, .. } if chat.kind == ConversationKind::Group => {
            use langis_store::GroupCallStatus as S;
            use proto::group_call::State;
            let state = match status {
                Some(S::Generic) | None => State::GENERIC,
                Some(S::OutgoingRing) => State::OUTGOING_RING,
                Some(S::Ringing) => State::RINGING,
                Some(S::Joined) => State::JOINED,
                Some(S::Accepted) => State::ACCEPTED,
                Some(S::Missed) => State::MISSED,
                Some(S::Declined) => State::DECLINED,
            };
            let started = recipients.person(by.as_deref());
            let call = proto::GroupCall {
                callId: call_id.map(|id| id as u64),
                state: state.into(),
                startedCallRecipientId: started,
                ringerRecipientId: started.filter(|_| matches!(status, Some(S::Ringing | S::Accepted | S::Declined | S::Missed | S::OutgoingRing))),
                startedCallTimestamp: sent_at.max(0) as u64,
                read: true,
                ..Default::default()
            };
            (started.unwrap_or(me), Update::GroupCall(call))
        }
        // Desktop does not export its default-timer notice either.
        Notice::GroupCall { .. } | Notice::ContactRemoved | Notice::Call { .. } | Notice::UniversalTimer { .. } => return None,
    })
}

fn group_change(updates: Vec<proto::group_change_chat_update::update::Update>) -> proto::chat_update_message::Update {
    proto::chat_update_message::Update::GroupChange(proto::GroupChangeChatUpdate {
        updates: updates
            .into_iter()
            .map(|u| proto::group_change_chat_update::Update { update: Some(u), ..Default::default() })
            .collect(),
        ..Default::default()
    })
}

/// A person (`None`: us) as an ACI field; `None` for an unknown actor.
fn aci_or_me(recipients: &Recipients, who: Option<&str>) -> Option<Vec<u8>> {
    match who {
        Some(langis_store::UNKNOWN_ACTOR) => None,
        Some(sid) => uuid_bytes(sid),
        None => uuid_bytes(&recipients.me),
    }
}

/// A group notice as the group change details Desktop's
/// `#fromGroupUpdateMessage` reads back into it.
fn group_updates(recipients: &Recipients, by: Option<&str>, change: &GroupNotice) -> Option<Vec<proto::group_change_chat_update::update::Update>> {
    use proto::group_change_chat_update::update::Update as U;
    let updater = aci_or_me(recipients, by);
    // A required ACI: a member (`None`: us).
    let aci = |who: &Option<String>| aci_or_me(recipients, who.as_deref());
    let inviter_aci = |inviter: &Inviter| match inviter {
        Inviter::Us => uuid_bytes(&recipients.me),
        Inviter::Person { id } => uuid_bytes(id),
        Inviter::Unknown => None,
    };
    let level = |admins_only: bool| {
        if admins_only { proto::GroupV2AccessLevel::ADMINISTRATOR } else { proto::GroupV2AccessLevel::MEMBER }.into()
    };
    let same = |member: &Option<String>| member.as_deref() == by;
    Some(vec![match change {
        GroupNotice::Title { title } => {
            U::GroupNameUpdate(proto::GroupNameUpdate { updaterAci: updater, newGroupName: Some(title.clone()), ..Default::default() })
        }
        GroupNotice::Description { description } => U::GroupDescriptionUpdate(proto::GroupDescriptionUpdate {
            updaterAci: updater,
            newDescription: description.clone(),
            ..Default::default()
        }),
        GroupNotice::Avatar => U::GroupAvatarUpdate(proto::GroupAvatarUpdate { updaterAci: updater, ..Default::default() }),
        GroupNotice::Added { member } if same(member) => {
            U::GroupMemberJoinedUpdate(proto::GroupMemberJoinedUpdate { newMemberAci: aci(member)?, ..Default::default() })
        }
        GroupNotice::Added { member } => U::GroupMemberAddedUpdate(proto::GroupMemberAddedUpdate {
            updaterAci: updater,
            newMemberAci: aci(member)?,
            ..Default::default()
        }),
        GroupNotice::Removed { member } if same(member) => {
            U::GroupMemberLeftUpdate(proto::GroupMemberLeftUpdate { aci: aci(member)?, ..Default::default() })
        }
        GroupNotice::Removed { member } => U::GroupMemberRemovedUpdate(proto::GroupMemberRemovedUpdate {
            removerAci: updater,
            removedAci: aci(member)?,
            ..Default::default()
        }),
        GroupNotice::Admin { member, admin } => U::GroupAdminStatusUpdate(proto::GroupAdminStatusUpdate {
            updaterAci: updater,
            memberAci: aci(member)?,
            wasAdminStatusGranted: *admin,
            ..Default::default()
        }),
        GroupNotice::InfoAccess { admins_only } => U::GroupAttributesAccessLevelChangeUpdate(proto::GroupAttributesAccessLevelChangeUpdate {
            updaterAci: updater,
            accessLevel: level(*admins_only),
            ..Default::default()
        }),
        GroupNotice::MemberAccess { admins_only } => U::GroupMembershipAccessLevelChangeUpdate(proto::GroupMembershipAccessLevelChangeUpdate {
            updaterAci: updater,
            accessLevel: level(*admins_only),
            ..Default::default()
        }),
        GroupNotice::LabelAccess { admins_only } => U::GroupMemberLabelAccessLevelChangeUpdate(proto::GroupMemberLabelAccessLevelChangeUpdate {
            updaterAci: updater,
            accessLevel: level(*admins_only),
            ..Default::default()
        }),
        GroupNotice::AnnouncementsOnly { on } => U::GroupAnnouncementOnlyChangeUpdate(proto::GroupAnnouncementOnlyChangeUpdate {
            updaterAci: updater,
            isAnnouncementOnly: *on,
            ..Default::default()
        }),
        GroupNotice::Ended => U::GroupTerminateChangeUpdate(proto::GroupTerminateChangeUpdate { updaterAci: updater, ..Default::default() }),
        GroupNotice::JoinedByLink { member } => {
            U::GroupMemberJoinedByLinkUpdate(proto::GroupMemberJoinedByLinkUpdate { newMemberAci: aci(member)?, ..Default::default() })
        }
        GroupNotice::AcceptedInvite { member, inviter } => U::GroupInvitationAcceptedUpdate(proto::GroupInvitationAcceptedUpdate {
            inviterAci: inviter_aci(inviter),
            newMemberAci: aci(member)?,
            ..Default::default()
        }),
        GroupNotice::Approved { member } => U::GroupJoinRequestApprovalUpdate(proto::GroupJoinRequestApprovalUpdate {
            requestorAci: aci(member)?,
            updaterAci: updater,
            wasApproved: true,
            ..Default::default()
        }),
        GroupNotice::Invited { member: None } => {
            U::SelfInvitedToGroupUpdate(proto::SelfInvitedToGroupUpdate { inviterAci: updater, ..Default::default() })
        }
        GroupNotice::Invited { member: Some(member) } if by.is_none() => {
            U::SelfInvitedOtherUserToGroupUpdate(proto::SelfInvitedOtherUserToGroupUpdate {
                inviteeServiceId: langis_store_service_id_bytes(member)?,
                ..Default::default()
            })
        }
        GroupNotice::Invited { member: Some(_) } => {
            U::GroupUnknownInviteeUpdate(proto::GroupUnknownInviteeUpdate { inviterAci: updater, inviteeCount: 1, ..Default::default() })
        }
        GroupNotice::InvitedMany { count } => {
            U::GroupUnknownInviteeUpdate(proto::GroupUnknownInviteeUpdate { inviterAci: updater, inviteeCount: *count, ..Default::default() })
        }
        GroupNotice::InviteRemoved { member: None, .. } => {
            U::GroupSelfInvitationRevokedUpdate(proto::GroupSelfInvitationRevokedUpdate { revokerAci: updater, ..Default::default() })
        }
        GroupNotice::InviteRemoved { member, inviter } if same(member) => {
            U::GroupInvitationDeclinedUpdate(proto::GroupInvitationDeclinedUpdate {
                inviterAci: inviter_aci(inviter),
                inviteeAci: aci(member),
                ..Default::default()
            })
        }
        GroupNotice::InviteRemoved { member, inviter } => U::GroupInvitationRevokedUpdate(proto::GroupInvitationRevokedUpdate {
            updaterAci: updater,
            invitees: vec![proto::group_invitation_revoked_update::Invitee {
                inviterAci: inviter_aci(inviter),
                inviteeAci: aci(member),
                ..Default::default()
            }],
            ..Default::default()
        }),
        GroupNotice::InvitesRemoved { count, inviter } => U::GroupInvitationRevokedUpdate(proto::GroupInvitationRevokedUpdate {
            updaterAci: updater,
            invitees: (0..(*count).max(2))
                .map(|_| proto::group_invitation_revoked_update::Invitee { inviterAci: inviter_aci(inviter), ..Default::default() })
                .collect(),
            ..Default::default()
        }),
        GroupNotice::Requested { member } => {
            U::GroupJoinRequestUpdate(proto::GroupJoinRequestUpdate { requestorAci: aci(member)?, ..Default::default() })
        }
        GroupNotice::RequestRemoved { member } if same(member) => {
            U::GroupJoinRequestCanceledUpdate(proto::GroupJoinRequestCanceledUpdate { requestorAci: aci(member)?, ..Default::default() })
        }
        GroupNotice::RequestRemoved { member } => U::GroupJoinRequestApprovalUpdate(proto::GroupJoinRequestApprovalUpdate {
            requestorAci: aci(member)?,
            updaterAci: updater,
            wasApproved: false,
            ..Default::default()
        }),
        GroupNotice::RequestBounce { member, times, pending } => {
            let requestor = uuid_bytes(member)?;
            let mut updates = vec![U::GroupSequenceOfRequestsAndCancelsUpdate(proto::GroupSequenceOfRequestsAndCancelsUpdate {
                requestorAci: requestor.clone(),
                count: *times,
                ..Default::default()
            })];
            if *pending {
                updates.push(U::GroupJoinRequestUpdate(proto::GroupJoinRequestUpdate { requestorAci: requestor, ..Default::default() }));
            }
            return Some(updates);
        }
        GroupNotice::LinkOn { approval } => U::GroupInviteLinkEnabledUpdate(proto::GroupInviteLinkEnabledUpdate {
            updaterAci: updater,
            linkRequiresAdminApproval: *approval,
            ..Default::default()
        }),
        GroupNotice::LinkOff => U::GroupInviteLinkDisabledUpdate(proto::GroupInviteLinkDisabledUpdate { updaterAci: updater, ..Default::default() }),
        GroupNotice::LinkReset => U::GroupInviteLinkResetUpdate(proto::GroupInviteLinkResetUpdate { updaterAci: updater, ..Default::default() }),
        GroupNotice::LinkApproval { on } => U::GroupInviteLinkAdminApprovalUpdate(proto::GroupInviteLinkAdminApprovalUpdate {
            updaterAci: updater,
            linkRequiresAdminApproval: *on,
            ..Default::default()
        }),
    }])
}

/// Gzips and encrypts a backup's frames as Desktop's `#runExport` does for
/// `encrypted`: the padded gzip ([`padded_gzip`]), then a random IV,
/// AES-256-CBC, HMAC-SHA256 over both, with the message backup key
/// (libsignal's `MessageBackupKey` from the backup key and our backup id).
pub(crate) fn encrypt(plain: &[u8], keys: &AccountKeys) -> anyhow::Result<Vec<u8>> {
    use libsignal_message_backup::key::MessageBackupKey;
    let backup_key = keys.backup_key()?;
    let backup_id = backup_key.derive_backup_id(&keys.aci);
    let key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    let mut data = padded_gzip(plain)?;
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(&key.aes_key);
    combined[32..].copy_from_slice(&key.hmac_key);
    presage::libsignal_service::attachment_cipher::encrypt_in_place(rand::random(), combined, &mut data);
    Ok(data)
}

/// Desktop's `toPaddedGzipIterable` (ts/services/backups/encoding.node.ts):
/// the stream gzipped with full flushes (so no DEFLATE block spans what
/// they separate) after the BackupInfo, around the account and recipient
/// frames and any other frame among the chat items, and through the chat
/// items at libsignal's `flush_interval`s; then zeros after the gzip,
/// `padding_size` of them from the largest chat item block, so the size
/// says little about what's inside.
fn padded_gzip(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::{Compress, Compression, FlushCompress};
    use libsignal_message_backup::padding::{flush_interval, padding_size};
    use protobuf::Message as _;

    /// Deflates `input` into `out`, all of it and then `flush`.
    fn deflate(z: &mut Compress, mut input: &[u8], out: &mut Vec<u8>, flush: FlushCompress) -> anyhow::Result<()> {
        loop {
            out.reserve((input.len() / 2).max(64 * 1024));
            let before = z.total_in();
            let status = z.compress_vec(input, out, flush)?;
            input = &input[(z.total_in() - before) as usize..];
            let done = match flush {
                FlushCompress::Finish => status == flate2::Status::StreamEnd,
                // Everything's in and the output wasn't cut short.
                _ => input.is_empty() && out.len() < out.capacity(),
            };
            if done {
                return Ok(());
            }
        }
    }

    // The delimited messages: BackupInfo, then each frame and its kind.
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Info,
        ChatItem,
        AccountOrRecipient,
        Other,
    }
    let mut pieces: Vec<(&[u8], Kind)> = Vec::new();
    let mut at = 0;
    while at < plain.len() {
        let (mut len, mut shift, start) = (0usize, 0, at);
        loop {
            let byte = *plain.get(at).context("a cut length")?;
            at += 1;
            len |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let body = plain.get(at..at + len).context("a cut frame")?;
        let kind = if start == 0 {
            Kind::Info
        } else {
            use proto::frame::Item;
            match proto::Frame::parse_from_bytes(body)?.item {
                Some(Item::ChatItem(_)) => Kind::ChatItem,
                Some(Item::Account(_) | Item::Recipient(_)) => Kind::AccountOrRecipient,
                _ => Kind::Other,
            }
        };
        pieces.push((&plain[start..at + len], kind));
        at += len;
    }

    let mut z = Compress::new(Compression::default(), false);
    let mut crc = flate2::Crc::new();
    // gzip's header: deflate, no name, no time, unknown OS.
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let mut pending: Vec<u8> = Vec::new();
    let mut flush = |pending: &mut Vec<u8>, out: &mut Vec<u8>, z: &mut Compress| -> anyhow::Result<()> {
        crc.update(pending);
        deflate(z, pending, out, FlushCompress::Full)?;
        pending.clear();
        Ok(())
    };
    let (mut largest, mut current, mut chat_bytes) = (0u64, 0u64, 0u64);
    let mut next_flush = flush_interval(0, None);
    for &(bytes, kind) in &pieces {
        if kind == Kind::Info {
            pending.extend_from_slice(bytes);
            flush(&mut pending, &mut out, &mut z)?;
            continue;
        }
        let chat = kind == Kind::ChatItem;
        let mut around = kind == Kind::AccountOrRecipient;
        if chat_bytes == 0 && chat {
            flush(&mut pending, &mut out, &mut z)?;
        } else if chat_bytes != 0 && !chat {
            around = true;
            largest = largest.max(current);
            current = 0;
        }
        if around {
            flush(&mut pending, &mut out, &mut z)?;
        }
        if !chat {
            pending.extend_from_slice(bytes);
        } else {
            let mut frame = bytes;
            while !frame.is_empty() {
                let cut = ((next_flush - chat_bytes) as usize).min(frame.len());
                pending.extend_from_slice(&frame[..cut]);
                chat_bytes += cut as u64;
                current += cut as u64;
                if cut == frame.len() {
                    break;
                }
                frame = &frame[cut..];
                next_flush += flush_interval(next_flush, None);
                largest = largest.max(current);
                current = 0;
                flush(&mut pending, &mut out, &mut z)?;
            }
        }
        if around {
            flush(&mut pending, &mut out, &mut z)?;
        }
    }
    crc.update(&pending);
    deflate(&mut z, &pending, &mut out, FlushCompress::Finish)?;
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&crc.amount().to_le_bytes());
    let padding = padding_size(largest, out.len() as u64, &mut rand::rng());
    out.resize(out.len() + padding as usize, 0);
    Ok(out)
}

/// A file encrypted for `files/` (Desktop's local attachment format, the
/// attachment format padded as attachments are).
fn encrypt_file(mut data: Vec<u8>, local_key: [u8; 64]) -> Vec<u8> {
    let padded = crate::long_text::log_pad(data.len() as u64) as usize;
    data.resize(padded.max(data.len()), 0);
    presage::libsignal_service::attachment_cipher::encrypt_in_place(rand::random(), local_key, &mut data);
    data
}

/// Desktop's `writeLocalBackupMetadata`: the backup id, AES-256-CTR with
/// the local backup metadata key and a 12-byte IV (a 4-byte zero counter
/// after it).
fn metadata(keys: &AccountKeys) -> anyhow::Result<Vec<u8>> {
    use aes::cipher::{KeyIvInit as _, StreamCipher as _};
    let backup_key = keys.backup_key()?;
    let backup_id = backup_key.derive_backup_id(&keys.aci);
    let metadata_key = backup_key.derive_local_backup_metadata_key();
    let iv: [u8; 12] = rand::random();
    let mut counter = [0u8; 16];
    counter[..12].copy_from_slice(&iv);
    let mut encrypted = backup_id.0.to_vec();
    ctr::Ctr32BE::<aes::Aes256>::new((&metadata_key).into(), (&counter).into()).apply_keystream(&mut encrypted);
    let message = local_proto::Metadata {
        version: LOCAL_BACKUP_VERSION,
        backup_id: Some(local_proto::EncryptedBackupId { iv: iv.to_vec(), encrypted_id: encrypted }),
    };
    Ok(prost::Message::encode_to_vec(&message))
}

/// Desktop's `verifyLocalBackupMetadata`: the metadata names this
/// account's backup id.
fn verify_metadata(bytes: &[u8], keys: &AccountKeys) -> anyhow::Result<()> {
    use aes::cipher::{KeyIvInit as _, StreamCipher as _};
    let metadata: local_proto::Metadata = prost::Message::decode(bytes)?;
    if metadata.version != LOCAL_BACKUP_VERSION {
        bail!("local backup version {} isn't {LOCAL_BACKUP_VERSION}", metadata.version);
    }
    let id = metadata.backup_id.context("no backup id")?;
    let iv: [u8; 12] = id.iv.as_slice().try_into().map_err(|_| anyhow!("bad IV"))?;
    let backup_key = keys.backup_key()?;
    let mut counter = [0u8; 16];
    counter[..12].copy_from_slice(&iv);
    let mut decrypted = id.encrypted_id;
    ctr::Ctr32BE::<aes::Aes256>::new((&backup_key.derive_local_backup_metadata_key()).into(), (&counter).into())
        .apply_keystream(&mut decrypted);
    if decrypted != backup_key.derive_backup_id(&keys.aci).0 {
        bail!("the backup id doesn't match");
    }
    Ok(())
}

/// Desktop's `writeLocalBackupFilesList`: a `FilesFrame` per file,
/// length-prefixed.
fn files_list(names: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for name in names {
        let frame = local_proto::FilesFrame { item: Some(local_proto::files_frame::Item::MediaName(name.clone())) };
        prost::Message::encode_length_delimited(&frame, &mut out).expect("a Vec grows");
    }
    out
}

/// Desktop's `readLocalBackupFilesList`.
fn read_files_list(mut bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    while !bytes.is_empty() {
        let frame: local_proto::FilesFrame = prost::Message::decode_length_delimited(&mut bytes)?;
        if let Some(local_proto::files_frame::Item::MediaName(name)) = frame.item {
            names.push(name);
        }
    }
    Ok(names)
}

/// Desktop's `LocalBackup.proto` (`signal.backup.local`).
mod local_proto {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Metadata {
        #[prost(uint32, tag = "1")]
        pub version: u32,
        #[prost(message, optional, tag = "2")]
        pub backup_id: Option<EncryptedBackupId>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct EncryptedBackupId {
        #[prost(bytes = "vec", tag = "1")]
        pub iv: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        pub encrypted_id: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FilesFrame {
        #[prost(oneof = "files_frame::Item", tags = "1")]
        pub item: Option<files_frame::Item>,
    }

    pub mod files_frame {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Item {
            #[prost(string, tag = "1")]
            MediaName(String),
        }
    }
}

/// Checks a backup with libsignal's validator, as Desktop's
/// `validateBackup` does before keeping one (purpose `RemoteBackup`:
/// "libsignal does not have a separate localBackup purpose").
pub(crate) fn validate(encrypted: &[u8], keys: &AccountKeys) -> anyhow::Result<()> {
    use libsignal_message_backup::{BackupReader, backup::Purpose, frame::CursorFactory, key::MessageBackupKey};
    let backup_key = keys.backup_key()?;
    let backup_id = backup_key.derive_backup_id(&keys.aci);
    let key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    futures::executor::block_on(async {
        let reader = BackupReader::new_encrypted_compressed(&key, CursorFactory::new(encrypted), Purpose::RemoteBackup)
            .await
            .map_err(|e| anyhow!("can't read the backup: {e}"))?;
        let result = reader.validate_all().await;
        result.result.map_err(|e| anyhow!("the backup isn't valid: {e}"))?;
        if !result.found_unknown_fields.is_empty() {
            tracing::warn!(fields = ?result.found_unknown_fields, "backup has unknown fields");
        }
        Ok(())
    })
}

/// The free space where `dir` is, when the system says.
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `stat` a valid out pointer.
    (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn io_error(error: &std::io::Error, still_to_write: u64) -> LocalBackupError {
    match error.raw_os_error() {
        Some(libc::ENOSPC) => LocalBackupError::RanOutOfStorage { bytes_needed: still_to_write },
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => LocalBackupError::Permissions,
        _ => LocalBackupError::General(error.to_string()),
    }
}

fn general(error: impl std::fmt::Display) -> LocalBackupError {
    LocalBackupError::General(error.to_string())
}

/// The secret file keys are derived from, made once.
async fn file_secret(db: &AppDb) -> anyhow::Result<[u8; 32]> {
    if let Some(secret) = db.setting(SETTING_FILE_SECRET).await?.and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok()) {
        return Ok(secret);
    }
    let secret: [u8; 32] = rand::random();
    db.set_setting(SETTING_FILE_SECRET, &hex::encode(secret)).await?;
    Ok(secret)
}

/// Makes a backup in `base` (a `SignalBackups` folder): Desktop's
/// `exportLocalBackup`. `progress` hears each step; `cancelled` is checked
/// between files. On failure the new snapshot is removed and the folder
/// pruned, as Desktop does.
pub(crate) async fn export(
    db: &AppDb,
    keys: &AccountKeys,
    base: &Path,
    now: i64,
    mut progress: impl FnMut(LocalBackupProgress),
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<PathBuf, LocalBackupError> {
    if keys.entropy_pool.is_empty() {
        return Err(LocalBackupError::NoRecoveryKey);
    }
    std::fs::create_dir_all(base).map_err(|e| io_error(&e, 0))?;
    if let Some(free) = free_bytes(base)
        && free < MINIMUM_FREE_BYTES
    {
        return Err(LocalBackupError::NotEnoughStorage { bytes_needed: MINIMUM_FREE_BYTES - free });
    }
    std::fs::create_dir_all(base.join("files")).map_err(|e| io_error(&e, 0))?;
    let snapshot = base.join(format!("signal-backup-{}", folder_timestamp(now)));
    if snapshot.exists() {
        return Err(general("a backup from this second is already there"));
    }
    std::fs::create_dir_all(&snapshot).map_err(|e| io_error(&e, 0))?;
    progress(LocalBackupProgress::Messages);
    let result = write_snapshot(db, keys, base, &snapshot, now, &mut progress, cancelled).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&snapshot);
    }
    if let Err(error) = prune(base, SNAPSHOTS_TO_KEEP) {
        tracing::warn!(%error, "failed to prune old local backups");
    }
    result.map(|()| snapshot)
}

async fn write_snapshot(
    db: &AppDb,
    keys: &AccountKeys,
    base: &Path,
    snapshot: &Path,
    now: i64,
    progress: &mut impl FnMut(LocalBackupProgress),
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), LocalBackupError> {
    use std::sync::atomic::Ordering;
    let secret = file_secret(db).await.map_err(general)?;
    let frames = frames(db, keys, &secret, now).await.map_err(general)?;
    tracing::info!(stats = ?frames.stats, "local backup frames");
    if cancelled.load(Ordering::Relaxed) {
        return Err(LocalBackupError::Cancelled);
    }
    let (plain, keys_owned) = (frames.plain, keys.clone());
    let main = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let encrypted = encrypt(&plain, &keys_owned)?;
        validate(&encrypted, &keys_owned)?;
        Ok(encrypted)
    })
    .await
    .map_err(general)?
    .map_err(general)?;
    std::fs::write(snapshot.join("main"), &main).map_err(|e| io_error(&e, main.len() as u64))?;
    let names: Vec<String> = frames.files.iter().map(|f| f.media_name.clone()).collect();
    std::fs::write(snapshot.join("files"), files_list(&names)).map_err(|e| io_error(&e, 0))?;
    let read = std::fs::read(snapshot.join("files")).map_err(|e| io_error(&e, 0)).and_then(|b| read_files_list(&b).map_err(general))?;
    if read != names {
        return Err(general("the files list doesn't read back as written"));
    }
    let metadata_bytes = metadata(keys).map_err(general)?;
    std::fs::write(snapshot.join("metadata"), &metadata_bytes).map_err(|e| io_error(&e, 0))?;
    verify_metadata(&std::fs::read(snapshot.join("metadata")).map_err(|e| io_error(&e, 0))?, keys).map_err(general)?;

    // Files (Desktop's `#runLocalAttachmentBackupJobs`): room for them all
    // and 100 MiB first, then each one not already there.
    let total: u64 = frames.files.iter().map(|f| f.size).sum();
    if let Some(free) = free_bytes(base)
        && free < total + ATTACHMENT_BUFFER_BYTES
    {
        return Err(LocalBackupError::NotEnoughStorage { bytes_needed: total + ATTACHMENT_BUFFER_BYTES - free });
    }
    let mut done = 0u64;
    progress(LocalBackupProgress::Attachments { done, total });
    for job in &frames.files {
        if cancelled.load(Ordering::Relaxed) {
            return Err(LocalBackupError::Cancelled);
        }
        let dir = base.join("files").join(&job.media_name[..2]);
        let target = dir.join(&job.media_name);
        if !target.exists() {
            let (source, key, dir, target) = (job.source.clone(), job.local_key, dir.clone(), target.clone());
            let written = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
                let data = match std::fs::read(&source) {
                    Ok(data) => data,
                    // Desktop's `AttachmentPermanentlyMissingError`: go on.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                };
                std::fs::create_dir_all(&dir)?;
                let temp = dir.join(format!("{}.tmp-{}", target.file_name().unwrap_or_default().to_string_lossy(), hex::encode(rand::random::<[u8; 8]>())));
                let result = std::fs::write(&temp, encrypt_file(data, key)).and_then(|()| std::fs::rename(&temp, &target));
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                result.map(|()| true)
            })
            .await
            .map_err(general)?;
            match written {
                Ok(true) => {}
                Ok(false) => tracing::warn!(media_name = %job.media_name, "attachment file missing; continuing"),
                Err(error) => return Err(io_error(&error, total - done)),
            }
        }
        done += job.size;
        progress(LocalBackupProgress::Attachments { done, total });
    }
    Ok(())
}

/// Desktop's `pruneLocalBackups`: keeps the newest `keep` snapshots and
/// the files they list; removes the rest.
fn prune(base: &Path, keep: usize) -> anyhow::Result<()> {
    let mut snapshots: Vec<PathBuf> = std::fs::read_dir(base)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| is_snapshot_name(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    snapshots.sort();
    snapshots.reverse();
    for old in snapshots.iter().skip(keep) {
        std::fs::remove_dir_all(old)?;
    }
    let mut referenced = std::collections::HashSet::new();
    for snapshot in snapshots.iter().take(keep) {
        if let Ok(bytes) = std::fs::read(snapshot.join("files")) {
            referenced.extend(read_files_list(&bytes)?);
        }
    }
    let files = base.join("files");
    let Ok(dirs) = std::fs::read_dir(&files) else { return Ok(()) };
    for dir in dirs.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
        for file in std::fs::read_dir(dir.path())?.filter_map(|e| e.ok()) {
            if file.file_type().is_ok_and(|t| t.is_file()) && !referenced.contains(&*file.file_name().to_string_lossy()) {
                std::fs::remove_file(file.path())?;
            }
        }
    }
    Ok(())
}

/// Desktop's `LOCAL_BACKUP_SNAPSHOT_DIR_PATTERN`.
fn is_snapshot_name(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix("signal-backup-") else { return false };
    let bytes = stamp.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(ix, b)| if [4, 7, 10, 13, 16].contains(&ix) { *b == b'-' } else { b.is_ascii_digit() })
}

/// Desktop's `pickLocalBackupFolder`: the `SignalBackups` folder in the
/// chosen one, made and kept.
pub(crate) async fn choose_folder(db: &AppDb, parent: &Path) -> anyhow::Result<PathBuf> {
    let folder = parent.join(DIR_NAME);
    std::fs::create_dir_all(&folder).with_context(|| format!("making {}", folder.display()))?;
    db.set_setting(SETTING_FOLDER, &folder.to_string_lossy()).await?;
    Ok(folder)
}

/// Desktop's `disableLocalBackups`: forgets the folder, the last backup and
/// the confirmed key, and with `delete` removes the folder, only when it's
/// named `SignalBackups`.
pub(crate) async fn disable(db: &AppDb, delete: bool) -> anyhow::Result<()> {
    let folder = db.setting(SETTING_FOLDER).await?;
    for key in [SETTING_LAST, SETTING_FOLDER, SETTING_KEY_VIEWED] {
        db.remove_setting(key).await?;
    }
    if delete && let Some(folder) = folder.map(PathBuf::from) {
        if folder.file_name().is_some_and(|n| n == DIR_NAME) {
            std::fs::remove_dir_all(&folder).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
        } else {
            tracing::warn!("backups folder isn't named {DIR_NAME}; not deleting it");
        }
    }
    Ok(())
}

/// Backups in progress, by data directory: whether each was cancelled.
static RUNNING: std::sync::Mutex<Option<HashMap<PathBuf, std::sync::Arc<std::sync::atomic::AtomicBool>>>> = std::sync::Mutex::new(None);

fn running() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, std::sync::Arc<std::sync::atomic::AtomicBool>>>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Desktop backup commands, for the backend and the demo alike;
/// `false` for any other command. `keys`: this account's, `None` when
/// this device has no recovery key.
pub(crate) async fn handle(
    db: &AppDb,
    data_dir: &Path,
    keys: Option<AccountKeys>,
    command: &crate::Command,
    events: &futures::channel::mpsc::UnboundedSender<crate::Event>,
) -> bool {
    use crate::{Command, Event};
    let emit = |event: Event| {
        let _ = events.unbounded_send(event);
    };
    match command {
        Command::LoadRecoveryKey => emit(Event::RecoveryKey(keys.map(|k| k.entropy_pool))),
        Command::SetLocalBackupFolder { parent } => match choose_folder(db, parent).await {
            Ok(_) => emit(Event::LocalBackupsChanged),
            Err(error) => emit(Event::Error(format!("couldn't use that folder: {error:#}"))),
        },
        Command::RecoveryKeyConfirmed => {
            let Some(keys) = keys else { return true };
            match db.set_setting(SETTING_KEY_VIEWED, &key_hash(&keys.entropy_pool)).await {
                Ok(()) => emit(Event::LocalBackupsChanged),
                Err(error) => emit(Event::Error(format!("couldn't save that: {error}"))),
            }
        }
        Command::DisableLocalBackups { delete } => {
            if let Some(flag) = running().as_ref().and_then(|r| r.get(data_dir)) {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Err(error) = disable(db, *delete).await {
                emit(Event::Error(format!("couldn't turn off backups: {error:#}")));
            }
            emit(Event::LocalBackupsChanged);
        }
        Command::CancelLocalBackup => {
            if let Some(flag) = running().as_ref().and_then(|r| r.get(data_dir)) {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Command::StartLocalBackup => {
            let Some(keys) = keys else {
                emit(Event::LocalBackup(LocalBackupProgress::Failed(LocalBackupError::NoRecoveryKey)));
                return true;
            };
            let Some(folder) = db.setting(SETTING_FOLDER).await.ok().flatten().map(PathBuf::from) else {
                emit(Event::LocalBackup(LocalBackupProgress::Failed(LocalBackupError::General("no backup folder chosen".into()))));
                return true;
            };
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let mut running = running();
                let running = running.get_or_insert_default();
                if running.contains_key(data_dir) {
                    // One at a time, as Desktop's `#runExclusiveExport`.
                    return true;
                }
                running.insert(data_dir.to_owned(), cancelled.clone());
            }
            let (db, data_dir, events) = (db.clone(), data_dir.to_owned(), events.clone());
            tokio::task::spawn_local(async move {
                let emit = |event: Event| {
                    let _ = events.unbounded_send(event);
                };
                let now = chrono::Utc::now().timestamp_millis();
                let result = export(&db, &keys, &folder, now, |p| emit(Event::LocalBackup(p)), &cancelled).await;
                if let Some(running) = running().as_mut() {
                    running.remove(&data_dir);
                }
                match result {
                    Ok(snapshot) => {
                        let _ = db.set_setting(SETTING_LAST, &format!("{now} {}", snapshot.to_string_lossy())).await;
                        emit(Event::LocalBackup(LocalBackupProgress::Complete { at: now, snapshot }));
                        emit(Event::LocalBackupsChanged);
                    }
                    Err(error) => {
                        tracing::warn!(?error, "local backup failed");
                        emit(Event::LocalBackup(LocalBackupProgress::Failed(error)));
                    }
                }
            });
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests;
