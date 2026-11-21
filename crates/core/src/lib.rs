//! Signal networking on a dedicated Tokio runtime.
//!
//! The UI never touches presage directly: it sends [`Command`]s and receives
//! [`Event`]s, and reads rendered state from [`langis_store::AppDb`].

use std::{future::Future, path::PathBuf};

use anyhow::Context as _;
use futures::channel::mpsc;
use langis_store::{AppDb, ConversationId};

pub use url::Url;

mod actor;
mod demo;
mod demo_groups;
pub mod mapping;
mod storage;
mod groups;
pub mod group_admin;
mod history;
mod backup_media;
pub mod backups;
pub mod local_backup;
mod safety;
mod key_transparency;
mod calling;
mod call_links;
pub mod profiles;
pub mod usernames;
pub mod media;
pub mod link_preview;
pub mod debug_log;
pub mod keys;
pub mod recipients;
pub mod giphy;
pub mod sticker_upload;
pub mod stickers;
pub mod deletes;
pub mod edits;
pub mod long_text;
pub mod release_notes;
pub mod call_survey;
pub mod megaphones;
pub mod remote_config;
pub mod video;
pub mod message_requests;
pub mod auto_download;
pub mod routes;
pub mod server_alerts;
pub mod share_contact;
mod sender_keys;
pub mod linking;
pub use linking::LinkError;

pub use remote_config::{ConfigCell, RemoteConfig};
pub(crate) use remote_config::current as remote_config;

/// Settings key: `"true"` when read receipts are on: sent, and others'
/// shown. See [`AccountSetting`] for where it comes from and its default.
pub const SETTING_READ_RECEIPTS: &str = "read_receipts";
/// Settings key: `"true"` when typing indicators are on: sent, and others'
/// shown.
pub const SETTING_TYPING_INDICATORS: &str = "typing_indicators";
/// Settings key: `"true"` when link previews are generated.
pub const SETTING_LINK_PREVIEWS: &str = "link_previews";
/// Settings key: `"true"` to show which messages came by sealed sender
/// (Desktop's "Show status icon", in message details).
pub const SETTING_SEALED_SENDER_INDICATORS: &str = "sealed_sender_indicators";
/// Settings key: `"true"` to show photos from the phone's address book
/// (Desktop's `preferContactAvatars`).
pub const SETTING_PREFER_CONTACT_AVATARS: &str = "prefer_contact_avatars";
/// Settings key: `"true"` to keep muted chats archived when a message comes
/// (Desktop's `keepMutedChatsArchived`). The store reads it.
pub const SETTING_KEEP_MUTED_CHATS_ARCHIVED: &str = langis_store::KEEP_MUTED_CHATS_ARCHIVED;
/// Settings key: notify when someone reacts to our message (Desktop's
/// `reaction-notification`; unset is on).
pub const SETTING_REACTION_NOTIFICATIONS: &str = "reaction_notifications";
/// Settings key: ring or notify for calls in muted chats (Desktop's
/// `notifyForCallsIfMuted`; unset is off).
pub const SETTING_NOTIFY_CALLS_IF_MUTED: &str = "notify_calls_if_muted";
/// Settings key: notify for mentions in muted chats (unset is on).
pub const SETTING_NOTIFY_MENTIONS_IF_MUTED: &str = "notify_mentions_if_muted";
/// Settings key: notify for replies to our messages in muted chats (unset
/// is on).
pub const SETTING_NOTIFY_REPLIES_IF_MUTED: &str = "notify_replies_if_muted";
/// Settings key: unread reminders for muted chats (Desktop's
/// `showUnreadReminders`; unset is off, `DEFAULT_SHOW_UNREAD_REMINDERS`).
pub const SETTING_SHOW_UNREAD_REMINDERS: &str = "show_unread_reminders";
/// Settings key: the default disappearing message timer for new chats, in
/// seconds (Desktop's `universalExpireTimer`; unset or `0` is off).
pub const SETTING_UNIVERSAL_EXPIRE_TIMER: &str = "universal_expire_timer";
/// Settings key: `"true"` when everybody can see our phone number, else
/// nobody can (Desktop's `phoneNumberSharingMode`; unset is nobody). Our
/// profile's `phoneNumberSharing` follows it.
pub const SETTING_PHONE_NUMBER_SHARING: &str = "account.sharing_number";
/// Settings key: `"true"` when people can find us by our number (Desktop's
/// `phoneNumberDiscoverability`, the account record's
/// `unlistedPhoneNumber` turned around). Unset counts as not.
pub const SETTING_PHONE_NUMBER_DISCOVERABLE: &str = key_transparency::SETTING_E164_DISCOVERABLE;
/// Settings key: `"false"` to turn incoming calls away (Desktop's "Enable
/// incoming calls", `incoming-call-notification`; on until turned off).
pub const SETTING_INCOMING_CALLS: &str = "incoming_calls";
/// Settings key: `"true"` to send every 1:1 call through Signal's relays
/// (Desktop's "Always relay calls", `always-relay-calls`).
pub const SETTING_ALWAYS_RELAY_CALLS: &str = "always_relay_calls";
/// Settings key: how screen shares are sent, as `ShareQuality::to_setting`
/// writes it (`1080@15`); unset is `ShareQuality::default()`. Set it with
/// `Command::SetShareQuality`.
pub const SETTING_SHARE_QUALITY: &str = "calls.share_quality";
/// Settings key: when the phone's contacts last came (unix millis),
/// Desktop's `synced_at`, shown as "Last import at".
pub const SETTING_CONTACTS_SYNCED_AT: &str = "contacts_synced_at";
/// Settings key: the photos the phone's address book has for people, as
/// a JSON object of service id to file (from the contact sync).
pub const SETTING_CONTACT_AVATARS: &str = "contact_avatars";
/// Settings key: the app's theme, `"system"`, `"light"` or `"dark"`
/// (Desktop's `theme-setting`). Not synced; the link-time backup sets it.
pub const SETTING_THEME: &str = "theme";

/// This device's own settings, saved with `Command::SetLocalSetting`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSetting {
    /// `SETTING_THEME`.
    Theme(Theme),
    /// `SETTING_INCOMING_CALLS`: off turns incoming calls away.
    IncomingCalls(bool),
    /// `SETTING_ALWAYS_RELAY_CALLS`.
    AlwaysRelayCalls(bool),
    /// `SETTING_IMAGE_OPTIMIZATION`: how hard images are shrunk before
    /// sending (Desktop's "Sent media quality" is the nearest setting).
    ImageOptimization(media::Optimization),
    /// `SETTING_GIPHY_API_KEY`: empty clears it, falling back to the
    /// `GIPHY_API_KEY` environment variable.
    GiphyKey(String),
}

impl LocalSetting {
    /// Its settings key and the value saved.
    pub fn key_value(&self) -> (&'static str, String) {
        let on = |on: bool| if on { "true" } else { "false" }.to_owned();
        match self {
            Self::Theme(theme) => (SETTING_THEME, theme.as_str().to_owned()),
            Self::IncomingCalls(value) => (SETTING_INCOMING_CALLS, on(*value)),
            Self::AlwaysRelayCalls(value) => (SETTING_ALWAYS_RELAY_CALLS, on(*value)),
            Self::ImageOptimization(level) => (SETTING_IMAGE_OPTIMIZATION, level.as_setting().to_owned()),
            Self::GiphyKey(key) => (SETTING_GIPHY_API_KEY, key.trim().to_owned()),
        }
    }
}

/// The app's theme (Desktop's `theme-setting`), as `SETTING_THEME` keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// From the saved value; anything else is `System`.
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }
}

/// One of the account's on/off settings that Signal syncs between devices
/// (the `AccountRecord` in Storage Service; the first four also in the
/// configuration sync message, and all but the notification ones in the
/// link-time backup). Like Desktop (`ts/util/Settings.preload.ts`,
/// `STORAGE_KEY_DEFAULTS`), each keeps its default until one of those says
/// otherwise; read receipts also take the phone's setting at linking
/// (`AccountManager`'s `createAccount`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AccountSetting {
    ReadReceipts,
    TypingIndicators,
    LinkPreviews,
    SealedSenderIndicators,
    PreferContactAvatars,
    KeepMutedChatsArchived,
    ReactionNotifications,
    NotifyCallsIfMuted,
    NotifyMentionsIfMuted,
    NotifyRepliesIfMuted,
    ShowUnreadReminders,
}

impl AccountSetting {
    pub const ALL: [AccountSetting; 11] = [
        Self::ReadReceipts,
        Self::TypingIndicators,
        Self::LinkPreviews,
        Self::SealedSenderIndicators,
        Self::PreferContactAvatars,
        Self::KeepMutedChatsArchived,
        Self::ReactionNotifications,
        Self::NotifyCallsIfMuted,
        Self::NotifyMentionsIfMuted,
        Self::NotifyRepliesIfMuted,
        Self::ShowUnreadReminders,
    ];

    /// Where it's kept in the app database's settings.
    pub fn key(self) -> &'static str {
        match self {
            Self::ReadReceipts => SETTING_READ_RECEIPTS,
            Self::TypingIndicators => SETTING_TYPING_INDICATORS,
            Self::LinkPreviews => SETTING_LINK_PREVIEWS,
            Self::SealedSenderIndicators => SETTING_SEALED_SENDER_INDICATORS,
            Self::PreferContactAvatars => SETTING_PREFER_CONTACT_AVATARS,
            Self::KeepMutedChatsArchived => SETTING_KEEP_MUTED_CHATS_ARCHIVED,
            Self::ReactionNotifications => SETTING_REACTION_NOTIFICATIONS,
            Self::NotifyCallsIfMuted => SETTING_NOTIFY_CALLS_IF_MUTED,
            Self::NotifyMentionsIfMuted => SETTING_NOTIFY_MENTIONS_IF_MUTED,
            Self::NotifyRepliesIfMuted => SETTING_NOTIFY_REPLIES_IF_MUTED,
            Self::ShowUnreadReminders => SETTING_SHOW_UNREAD_REMINDERS,
        }
    }

    /// Its value until something sets it: Desktop's defaults
    /// (`STORAGE_KEY_DEFAULTS`, `DEFAULT_NOTIFY_IF_MUTED`,
    /// `DEFAULT_SHOW_UNREAD_REMINDERS`).
    pub fn default_on(self) -> bool {
        matches!(self, Self::ReactionNotifications | Self::NotifyMentionsIfMuted | Self::NotifyRepliesIfMuted)
    }

    /// The account setting a chat's own `kind` falls back to.
    pub fn for_chat(kind: langis_store::ChatNotifyKind) -> Self {
        use langis_store::ChatNotifyKind as K;
        match kind {
            K::Calls => Self::NotifyCallsIfMuted,
            K::Mentions => Self::NotifyMentionsIfMuted,
            K::Replies => Self::NotifyRepliesIfMuted,
            K::UnreadReminders => Self::ShowUnreadReminders,
        }
    }

    /// Whether it's on, from its stored value.
    pub fn is_on(self, value: Option<&str>) -> bool {
        match value {
            Some("true") => true,
            Some("false") => false,
            _ => self.default_on(),
        }
    }
}

/// Whether a stored setting that's off until set is on: only `"true"` is.
pub fn setting_on(value: Option<&str>) -> bool {
    value == Some("true")
}

/// Account settings as one source gives them; what it leaves out stays as
/// it is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountSettings {
    /// On/off settings.
    pub flags: std::collections::BTreeMap<AccountSetting, bool>,
    /// The default disappearing timer for new chats, in seconds (0: off).
    pub universal_expire_timer: Option<u32>,
    /// Everybody can see our number (`true`) or nobody can.
    pub phone_number_sharing: Option<bool>,
    /// The reaction bar's emoji as the account keeps them (Desktop's
    /// `preferredReactionEmoji`; missing ones are the defaults).
    pub preferred_reactions: Option<Vec<String>>,
}

/// Settings key: the account's preferred reactions, a JSON list of emoji
/// ([`AccountSettings::preferred_reactions`]).
pub const SETTING_PREFERRED_REACTIONS: &str = "preferred_reaction_emoji";

/// Desktop's `canBeSynced` for preferred reactions: at most 20, each at
/// most 20 UTF-16 units.
pub fn preferred_reactions_syncable(list: &[String]) -> bool {
    list.len() <= 20 && list.iter().all(|e| e.encode_utf16().count() <= 20)
}

impl AccountSettings {
    /// Only `setting`, `on`.
    pub fn flag(setting: AccountSetting, on: bool) -> Self {
        let mut settings = Self::default();
        settings.flags.insert(setting, on);
        settings
    }

    /// Each on/off setting this gives a value for.
    pub fn values(&self) -> impl Iterator<Item = (AccountSetting, bool)> + '_ {
        self.flags.iter().map(|(setting, on)| (*setting, *on))
    }
}

/// Settings key: your Giphy API key for GIF search (`GIPHY_API_KEY` also
/// works). Not a Signal setting.
pub const SETTING_GIPHY_API_KEY: &str = "giphy_api_key";

/// Settings key: when `"true"`, stories are turned off (Desktop's
/// `hasStoriesDisabled`): none are kept or shown.
pub const SETTING_STORIES_DISABLED: &str = "stories_disabled";
/// Settings key: `"false"` to not tell people we viewed their stories
/// (Desktop's `storyViewReceiptsEnabled`; unset follows read receipts).
pub const SETTING_STORY_VIEW_RECEIPTS: &str = "story_view_receipts";

/// Settings key: lossless optimization of images we send (`max`, `fast` or
/// `off`; see [`media::Optimization`]). Not a Signal setting.
pub const SETTING_IMAGE_OPTIMIZATION: &str = "image_optimization";

/// Settings key: Desktop's "Sent media quality", `high` or `standard`
/// (unset is Standard): photos we send are scaled as Desktop's
/// `scaleImageToLevel` scales them ([`media::scale_to_level`]).
pub const SETTING_SENT_MEDIA_QUALITY: &str = "sent_media_quality";

pub use link_preview::{LinkPreviewDraft, PreviewImage};

/// Settings key: `"true"` when automatic key verification is off.
pub const SETTING_KEY_TRANSPARENCY_DISABLED: &str = key_transparency::SETTING_DISABLED;
/// Settings key: `"true"` once Desktop's key transparency introduction was seen.
pub const SETTING_KEY_TRANSPARENCY_ONBOARDING_SEEN: &str = key_transparency::SETTING_ONBOARDING_SEEN;
/// Which of Desktop's placeholder gradients a chat's withheld photo gets
/// (`getIdentifierHash`): a person's from their service id, a group's from
/// its group id.
pub fn photo_placeholder_hash(chat: &langis_store::ConversationId) -> u8 {
    match chat.as_group().and_then(|key| <[u8; 32]>::try_from(key).ok()) {
        Some(key) => langis_store::identifier_hash_of_bytes(&groups::group_identifier(key)),
        None => langis_store::identifier_hash(chat.as_direct().unwrap_or(&chat.0)),
    }
}

/// A new notification profile's id: 16 random bytes as lowercase hex
/// (Desktop's `generateNotificationProfileId`).
pub fn new_notification_profile_id() -> String {
    hex::encode(rand::random::<[u8; langis_store::NOTIFICATION_PROFILE_ID_LENGTH]>())
}

/// Whether key transparency is on at all (Desktop's remote config flag
/// `desktop.keyTransparency.prod`).
pub fn key_transparency_on(config: &RemoteConfig) -> bool {
    key_transparency::feature_on(config)
}

/// TLS for Signal's own servers (the calling server, updates2.signal.org),
/// whose certificates come from Signal's root CA: trusting only that, as
/// Desktop does.
pub(crate) fn signal_tls() -> anyhow::Result<rustls::ClientConfig> {
    use rustls::pki_types::{CertificateDer, pem::PemObject as _};
    let pem = presage::libsignal_service::configuration::ServiceConfiguration::from(presage::libsignal_service::configuration::SignalServers::Production)
        .certificate_authority;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(cert?)?;
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding `signal.db` (presage protocol state), `app.db` and attachments.
    pub data_dir: PathBuf,
    /// Raw SQLCipher key for both databases.
    pub db_key: [u8; 32],
    /// Device name shown in the phone's "Linked devices" list.
    pub device_name: String,
    /// Demo mode: sample data, never connects to Signal (see `demo.rs`).
    pub demo: bool,
    /// Directories this profile's data was in before the frontend moved it
    /// to `data_dir` (usually empty): the file paths stored under each are
    /// rewritten once to name `data_dir` instead.
    pub former_data_dirs: Vec<PathBuf>,
    /// Every account's calls, shared by their backends: while one is in a
    /// call, the others turn calls away as busy.
    pub call_lines: CallLines,
}

/// Which accounts (by data directory) are in a call. RingRTC answers a
/// call with "busy" during another only within one account's backend;
/// this extends that to the app's other accounts.
#[derive(Debug, Clone, Default)]
pub struct CallLines(std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>);

impl CallLines {
    /// Notes whether `line`'s account is in a call (or calling).
    pub(crate) fn set(&self, line: &std::path::Path, busy: bool) {
        let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if busy {
            lines.insert(line.to_owned());
        } else {
            lines.remove(line);
        }
    }

    /// Whether an account other than `line`'s is in a call.
    pub(crate) fn busy_elsewhere(&self, line: &std::path::Path) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|other| other != line)
    }
}

/// A group a link leads to, as Desktop's `GroupV2JoinDialog` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLinkPreview {
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub member_count: u32,
    /// An admin approves who joins.
    pub approval: bool,
    /// The group's photo, downloaded.
    pub avatar: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Starts linking as a secondary device. Emits `Event::LinkUrl` then `Event::Linked`.
    Link,
    /// The answer to `Event::LinkedOtherAccount`: `true` deletes the data
    /// here and finishes linking; `false` drops the new link (the phone
    /// forgets this device) and emits `Event::NeedsLinking`.
    ContinueLink { delete_data: bool },
    SendText {
        conversation: ConversationId,
        /// With a [`mapping::MENTION_PLACEHOLDER`] per @mention.
        body: String,
        /// Message id (in `AppDb`) being replied to.
        quote: Option<i64>,
        /// A preview fetched with [`Command::FetchLinkPreview`], to send along.
        preview: Option<LinkPreviewDraft>,
        /// Who each placeholder in `body` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `body` (placeholders and all).
        styles: Vec<langis_store::StyleSpan>,
    },
    /// GIF search (trending when `query` is empty); answered with `Event::Gifs`.
    SearchGifs { query: String, offset: u32 },
    /// Sends a GIF from search results, replying to `quote` (our row id)
    /// when given.
    SendGif {
        conversation: ConversationId,
        gif: giphy::Gif,
        quote: Option<i64>,
        /// The text sent with it (Desktop stages a GIF in the composer),
        /// with its mentions and formatting as `SendFile`'s caption.
        caption: String,
        mentions: Vec<String>,
        styles: Vec<langis_store::StyleSpan>,
    },
    /// Lists installed sticker packs; answered with `Event::Stickers`.
    LoadStickers,
    /// Desktop's sticker manager opened: answered with
    /// `Event::StickerManager`, again as packs' titles and covers arrive.
    LoadStickerManager,
    /// Opens a pack (Desktop's `StickerPreviewModal`, from the manager or a
    /// `signal.art` link): answered with `Event::StickerPackPreview`.
    PreviewStickerPack { id: Vec<u8>, key: Vec<u8> },
    /// Finds the pack of a sticker message stored before its attachment
    /// kept the pack, from the message presage stored: answered with
    /// `Event::MessageStickerPack`.
    FindMessageStickerPack { message_id: i64 },
    /// Installs a pack, here and on our other devices (a
    /// `StickerPackOperation` sync and its `InstalledStickerPackRecord`).
    InstallStickerPack { id: Vec<u8>, key: Vec<u8> },
    /// Uninstalls a pack, here and on our other devices.
    UninstallStickerPack { id: Vec<u8>, key: Vec<u8> },
    /// Installed packs in this order (Desktop's drag to reorder), saved for
    /// our other devices.
    OrderStickerPacks(Vec<Vec<u8>>),
    /// Hides a received pack from the sticker manager's "Shared With You",
    /// or (`hidden: false`, the toast's Undo) shows it again, here and on
    /// our other devices (its `HiddenStickerPackRecord`; Desktop's
    /// `markStickerPackAsHidden`). Hiding an installed pack uninstalls it
    /// first; installing a pack shows it again. Answered with
    /// `Event::StickerPackHidden`. Offered only while
    /// `RemoteConfig::sticker_pack_hiding` is on.
    SetStickerPackHidden { id: Vec<u8>, key: Vec<u8>, hidden: bool },
    /// The answers to `Event::CallQualitySurvey`, sent to Signal with the
    /// call's statistics (Desktop's `submitCallQualitySurvey`): whether it
    /// went well, the issues as Desktop's `CallQualitySurvey.Issue` values
    /// (`audio`, `audio_stuttering`, `audio_local_echo`,
    /// `audio_remote_echo`, `audio_drop`, `video`, `video_no_camera`,
    /// `video_low_quality`, `video_low_resolution`, `call_dropped`,
    /// `other`), the description that goes with `other`, and an uploaded
    /// debug log's URL. Answered with `Event::CallQualitySurveySent`.
    SubmitCallQualitySurvey {
        survey: call_survey::CallSurvey,
        satisfied: bool,
        issues: Vec<String>,
        description: Option<String>,
        debug_log_url: Option<String>,
    },
    /// Favorites a sticker, or unfavorites it, here and on our other
    /// devices (its `FavoriteStickerRecord`; Desktop's
    /// `markStickerAsFavorite`). The pack needn't be installed.
    SetStickerFavorite { pack_id: Vec<u8>, pack_key: Vec<u8>, sticker_id: u32, favorite: bool },
    /// Uploads a new sticker pack and installs it; answered with
    /// `Event::StickerUpload`.
    UploadStickerPack(sticker_upload::NewStickerPack),
    /// Sends a sticker from an installed pack, replying to `quote` (our
    /// row id) when given.
    SendSticker {
        conversation: ConversationId,
        pack: std::sync::Arc<StickerPackView>,
        sticker: StickerView,
        quote: Option<i64>,
    },
    /// Looks up a username or phone number on Signal; answered with
    /// `Event::RecipientFound`.
    FindRecipient { query: String },
    /// Builds a link preview for `url`; answered with `Event::LinkPreview`.
    FetchLinkPreview { url: String },
    /// Sends images (optimized losslessly first), with `caption` as the
    /// message text.
    SendImages {
        conversation: ConversationId,
        images: Vec<OutgoingImage>,
        caption: String,
        quote: Option<i64>,
        /// Who each placeholder in `caption` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `caption` (placeholders and all).
        styles: Vec<langis_store::StyleSpan>,
    },
    /// Sends any file as it is (not optimized), with `caption` as the
    /// message text.
    SendFile {
        conversation: ConversationId,
        file: OutgoingFile,
        caption: String,
        quote: Option<i64>,
        /// Who each placeholder in `caption` mentions, in order.
        mentions: Vec<String>,
        /// Its formatting: spans over `caption` (placeholders and all).
        styles: Vec<langis_store::StyleSpan>,
    },
    /// Reacts to message `message_id`; `remove` retracts our reaction.
    React { message_id: i64, emoji: String, remove: bool },
    /// Sends a poll (Desktop's "New poll"); empty options are left out.
    SendPoll { conversation: ConversationId, question: String, options: Vec<String>, allow_multiple: bool },
    /// Shares a contact in a chat (Desktop's `enqueueSharedContactForSend`):
    /// a message with only the contact, its photo copied and uploaded.
    ShareContact { conversation: ConversationId, share: Box<share_contact::ShareContact> },
    /// Votes in poll `message_id` for these options (none takes our vote
    /// back).
    VotePoll { message_id: i64, options: Vec<u32> },
    /// Ends our poll `message_id` for everyone.
    EndPoll { message_id: i64 },
    /// Marks a conversation read locally and sends read receipts / sync.
    MarkRead { conversation: ConversationId },
    /// A message was played (a voice message): viewed here, on our other
    /// devices, and (with read receipts on) to its author, once (Desktop's
    /// `markViewed`). Anything but a voice message is ignored.
    MarkViewed { message_id: i64 },
    /// Sends a typing indicator.
    Typing { conversation: ConversationId, started: bool },
    /// Deletes messages on this device and your other devices.
    DeleteForMe { message_ids: Vec<i64> },
    /// Pins a message for everyone in the chat; `duration` in seconds,
    /// `None` for forever.
    PinMessage { message_id: i64, duration: Option<u32> },
    UnpinMessage { message_id: i64 },
    /// Changes the text of one of our sent messages (Signal's edit).
    /// `preview` replaces the message's link preview; the same link keeps
    /// the one it has.
    EditMessage {
        message_id: i64,
        body: String,
        mentions: Vec<String>,
        styles: Vec<langis_store::StyleSpan>,
        preview: Option<LinkPreviewDraft>,
    },
    /// Sends a failed message again, with its original timestamp.
    RetrySend { message_id: i64 },
    /// Sends a failed edit again (Desktop's "Send again"): the message as
    /// it now reads, as a new edit.
    RetryEdit { message_id: i64 },
    /// Sends copies of messages (text, photos, GIFs, link previews) to
    /// other chats, as Desktop's "Forward". `body`, for a single message,
    /// is its text as edited before sending (Desktop's
    /// `ForwardMessageEditor`); `None` sends it as it was.
    Forward { message_ids: Vec<i64>, conversations: Vec<ConversationId>, body: Option<String> },
    /// Deletes a whole chat on this device and your other devices.
    DeleteConversation { conversation: ConversationId },
    /// Answers a message request as Desktop's message request bar does:
    /// `Accept` (also what Unblock does there), `Delete`, `Block`,
    /// `Spam` (Report spam) or `BlockAndSpam` (Report and block). Here
    /// and on our other devices; a spam report goes to Signal. Answered
    /// with `Event::MessageRequestAnswered`.
    AnswerMessageRequest { conversation: ConversationId, answer: langis_store::MessageRequestResponse },
    /// Deletes our own message for everyone.
    DeleteForEveryone { message_id: i64 },
    /// Sends a delete for everyone that failed again (Desktop's "Retry
    /// Delete for Everyone").
    RetryDeleteForEveryone { message_id: i64 },
    /// Downloads attachment `index` of `message_id`.
    FetchAttachment { message_id: i64, index: usize },
    /// Downloads every attachment of `message_id` not yet downloaded, one at
    /// a time (Desktop's "Download N items" on a grid).
    FetchAttachments { message_id: i64 },
    /// Downloads a message's link preview image, when the photos
    /// auto-download setting left it (Desktop downloads it on a click).
    FetchPreviewImage { message_id: i64 },
    /// Settings > Backups opened (Desktop's `refreshBackupSubscriptionStatus`,
    /// and with `details`, its details page's `refreshCloudBackupStatus`
    /// too): answered with `Event::BackupStatus` at once from what's stored,
    /// then again once the server answered. Each fetch runs at most every
    /// 30 seconds.
    RefreshBackupStatus { details: bool },
    /// Asks for `Event::BackupMediaDownload` (it also comes by itself as
    /// the download moves).
    LoadBackupMediaDownload,
    /// Pauses, resumes, cancels the backup media download, or closes its
    /// banner. Answered with `Event::BackupMediaDownload`.
    BackupMediaDownload(backups::BackupMediaDownloadAction),
    /// Stops attachment `index` of `message_id` downloading (Desktop's
    /// cancel on a download in progress); it's left not downloaded.
    CancelDownload { message_id: i64, index: usize },
    /// Opens their view-once message, its one time: answered with
    /// `Event::ViewOnceOpened`, after which it's erased.
    OpenViewOnce { message_id: i64 },
    /// Sends a photo or video to be viewed once (Desktop's view-once
    /// toggle): no caption, no quote.
    SendViewOnce { conversation: ConversationId, media: OutgoingViewOnce },
    /// Downloads the whole text of a long message whose download failed
    /// (Desktop's "Download Full Message").
    FetchLongText { message_id: i64 },
    /// Pins or unpins a conversation for the whole account (Storage Service).
    SetPinned { conversation: ConversationId, pinned: bool },
    /// Archives, mutes, marks unread or blocks a chat for the whole account
    /// (Storage Service).
    SetChat { conversation: ConversationId, change: ChatChange },
    /// Sets (or, all empty, deletes) the nickname and note we give someone
    /// in a 1:1 chat, here and for the whole account (Storage Service), as
    /// Desktop's "Nickname" dialog does. `plain_name` is ignored.
    SetNickname { conversation: ConversationId, nickname: langis_store::ContactNickname },
    /// Sets a chat's disappearing-message timer (seconds; 0 turns it off)
    /// for everyone in it.
    SetExpireTimer { conversation: ConversationId, seconds: u32 },
    /// Renames a group or changes its description (`None`: unchanged).
    UpdateGroup { conversation: ConversationId, title: Option<String>, description: Option<String> },
    /// Leaves a group.
    LeaveGroup { conversation: ConversationId },
    /// Creates a group with us as its admin; answered with `Event::GroupCreated`.
    /// `avatar` is a picked image (made a group photo as Desktop does) and
    /// `expire_timer` the disappearing message timer, in seconds (0: off).
    CreateGroup { title: String, members: Vec<String>, avatar: Option<Vec<u8>>, expire_timer: u32 },
    /// Sets the group's photo from a picked image, or removes it (`None`).
    SetGroupAvatar { conversation: ConversationId, image: Option<Vec<u8>> },
    /// Removes someone from a group, as Desktop's `removeFromGroupV2` does:
    /// a member (banned from rejoining by the link), an invitation, or a
    /// request to join (turned down). `service_id` is an ACI, or a PNI for
    /// an invitation.
    RemoveFromGroup { conversation: ConversationId, service_id: String },
    /// Makes a member an admin, or not (Desktop's `toggleAdmin`).
    SetGroupAdmin { conversation: ConversationId, member: String, admin: bool },
    /// Who may edit details, add people or send messages.
    SetGroupPermission { conversation: ConversationId, permission: group_admin::Permission },
    /// Turns the group link on or off, resets it, or sets admin approval.
    ChangeGroupLink { conversation: ConversationId, change: group_admin::LinkChange },
    /// Approves or turns down a request to join (an ACI).
    AnswerJoinRequest { conversation: ConversationId, member: String, approve: bool },
    /// Bans someone (an ACI) from joining or asking to join by the link,
    /// turning down a request they have open: the "Block request" on a
    /// `GroupNotice::RequestBounce` (Desktop's `blockGroupLinkRequests`).
    BlockGroupLinkRequests { conversation: ConversationId, member: String },
    /// Revokes invitations (service ids).
    RevokeGroupInvites { conversation: ConversationId, invitees: Vec<String> },
    /// Takes back our request to join a group.
    CancelJoinRequest { conversation: ConversationId },
    /// A `signal.group` link was opened: answered with
    /// `Event::GroupLinkPreview` (Desktop's join dialog), `Event::GroupOpened`
    /// when we're already in (or asking to join) the group, or
    /// `Event::GroupLinkFailed`.
    OpenGroupLink { url: String },
    /// Joins (or asks to join) the group a link names; answered with
    /// `Event::GroupOpened` or `Event::GroupLinkFailed`.
    JoinGroupLink { url: String },
    /// Fetches a group as the server has it now, as Desktop does when a
    /// group's chat or details open (`fetchLatestGroupV2Data`). At most
    /// every few minutes per group.
    RefreshGroup { conversation: ConversationId },
    /// Adds people (ACIs) to a group, inviting those whose profile key we
    /// don't have (Desktop's "Add members").
    AddGroupMembers { conversation: ConversationId, members: Vec<String> },
    /// Ends a group for everyone (an admin's "End group"), behind the remote
    /// config's `groupTerminate.send`.
    EndGroup { conversation: ConversationId },
    /// Sets (or with an empty `text`, clears) our label in a group.
    SetMemberLabel { conversation: ConversationId, emoji: Option<String>, text: String },
    /// Computes a 1:1 chat's safety number; answered with `Event::SafetyNumber`.
    LoadSafetyNumber { conversation: ConversationId },
    /// Marks a 1:1 contact's current identity verified (or clears it), here
    /// and on our other devices; answered with `Event::SafetyNumber`.
    SetVerified { conversation: ConversationId, verified: bool },
    /// Desktop's "Verify automatically" in their safety number screen: a
    /// key transparency check of them (and of us, until that has run once).
    /// Answered with `Event::KeyTransparency`.
    CheckKeyTransparency { conversation: ConversationId },
    /// The safety number screen closed: its key transparency check stops.
    CancelKeyTransparency,
    /// Automatic key verification off (`true`) or on, for the whole account
    /// (`AccountRecord.automaticKeyVerificationDisabled`). Off forgets what
    /// was verified.
    SetKeyTransparencyDisabled(bool),
    /// A megaphone's button: done with it (donate, finish) or put off
    /// (snooze). Answered with `Event::Megaphones`.
    PressMegaphone { id: String, cta: megaphones::MegaphoneCta },
    /// A story was shown: viewed here, on our other devices, and (with
    /// view receipts on) to its author.
    ViewStory { id: i64 },
    /// Downloads a story's photo or video.
    FetchStory { id: i64 },
    /// Replies to someone's story (or reacts, with `emoji`): one to one in
    /// your chat with them; a group's, with the story.
    ReplyToStory { id: i64, body: String, emoji: Option<String> },
    /// Sends our story again to those its send didn't reach
    /// (`Story::failed`; Desktop's "Click to retry").
    RetryStory { id: i64 },
    /// Posts a story to each of `targets`.
    PostStory { story: OutgoingStory, targets: Vec<StoryTarget> },
    /// Saves a story list (My Story's privacy, or a custom story), here and
    /// in Storage Service; a new id adds one.
    SaveStoryList(StoryList),
    /// Deletes a custom story list, and the stories sent to it for
    /// everyone (Desktop's `deleteDistributionList`).
    DeleteStoryList { id: String },
    /// Turns stories off (deleting the ones we sent, for everyone) or on,
    /// for the whole account (`AccountRecord.storiesDisabled`).
    SetStoriesDisabled(bool),
    /// Story view receipts on or off, for the whole account
    /// (`AccountRecord.storyViewReceiptsEnabled`).
    SetStoryViewReceipts(bool),
    /// Read receipts, typing indicators and the like on or off, for the
    /// whole account (Desktop's Privacy settings, kept in the
    /// `AccountRecord`).
    SetAccountSetting { setting: AccountSetting, on: bool },
    /// The default disappearing timer for chats we start, in seconds (0:
    /// off), for the whole account (`AccountRecord.universalExpireTimer`).
    SetUniversalExpireTimer(u32),
    /// The reaction bar's six emoji, for the whole account
    /// (`AccountRecord.preferredReactionEmoji`, Desktop's
    /// `savePreferredReactions`).
    SetPreferredReactions(Vec<String>),
    /// Who can see our number: everybody (`true`) or nobody, for the whole
    /// account and in our profile (Desktop's "Who can see my number").
    SetPhoneNumberSharing(bool),
    /// Whether people can find us by our number (Desktop's "Who can find
    /// me by number"), on the server and for the whole account.
    SetPhoneNumberDiscoverable(bool),
    /// Desktop's "Import now" (Chats): asks the phone for its contacts,
    /// configuration and blocked list (`sendSyncRequests`). The contacts'
    /// arrival is `Event::ContactsSynced`.
    SyncContacts,
    /// Desktop's media auto-download choices (Data usage), kept on this
    /// device.
    SetAutoDownload(auto_download::AutoDownload),
    /// Saves one of this device's own settings (not synced): the theme, a
    /// call setting, image optimization or the Giphy key. The core reads
    /// them as it needs them; no event.
    SetLocalSetting(LocalSetting),
    /// Adds groups' stories to the list we post to (`Always`, Desktop's
    /// "New group story") or removes them (`Never`, "Remove group story"),
    /// for the whole account (`GroupV2Record.storySendMode`).
    SetStorySendMode { groups: Vec<ConversationId>, mode: langis_store::StorySendMode },
    /// Deletes one of our stories for everyone who got it (Desktop's
    /// `deleteStoryForEveryone`).
    DeleteStory { id: i64 },
    /// Deletes a reply to a group story: here only, or (ours) for everyone
    /// in the group (Desktop's "Delete for me" / "Delete for everyone").
    DeleteStoryReply { story: i64, author: Option<String>, sent_at: i64, for_everyone: bool },
    /// Calls a 1:1 chat (Desktop's call buttons, after the lobby).
    StartCall { conversation: ConversationId, video: bool },
    /// Answers the ringing call, with our camera on or off.
    AcceptCall { video: bool },
    /// Declines the ringing call.
    DeclineCall,
    /// Ends the call (or stops calling).
    HangUp,
    /// Mutes (`false`) or unmutes our microphone.
    SetCallAudio(bool),
    /// Turns our camera on or off: in a call, or for the lobby's preview.
    SetCallVideo(bool),
    /// Who's in a group's call, for the lobby; answered with `Event::GroupPeek`.
    /// For a call link's call (`ConversationId::call_link`) too, which
    /// also refreshes the link's name and settings.
    PeekGroupCall { conversation: ConversationId },
    /// Whether a group call we start rings the group (Desktop's lobby Ring
    /// toggle) or only notifies it. On until changed; groups over 16 are
    /// never rung.
    SetOutgoingRing(bool),
    /// Makes a call link (Desktop's "Create a call link"): answered with
    /// `Event::CallLinkCreated`, once the calling server has it.
    CreateCallLink,
    /// Renames a link of ours, or changes whether an admin approves who
    /// joins (Desktop's call link details).
    UpdateCallLink { room_id: String, name: Option<String>, restrictions: Option<langis_store::CallLinkRestrictions> },
    /// Deletes a link of ours for everyone, or forgets one we only joined.
    DeleteCallLink { room_id: String },
    /// A `https://signal.link/call/#key=…` URL: keeps the link and reads
    /// its name; answered with `Event::CallLinkOpened` (or an error).
    OpenCallLink { url: String },
    /// Lets someone waiting into our call link's call (their ACI), or not.
    ApproveCallJoin { aci: String, approve: bool },
    /// Takes someone out of our call link's call; `block` keeps them out
    /// of it (RingRTC's `block_client`, Desktop's "Block from call").
    RemoveFromCall { demux_id: u32, block: bool },
    /// Lists what can be presented; answered with `Event::ShareSources`.
    /// On Linux that's one source (`langis_calls::PORTAL_SOURCE`) when the
    /// desktop has a ScreenCast portal, and none otherwise.
    LoadShareSources,
    /// Presents (a source id from `Event::ShareSources`). On Linux the
    /// desktop's picker opens; the call shows `sharing` once the user picks
    /// something, and nothing changes if they cancel. A share the desktop
    /// ends (its indicator, a closed window) stops as `StopSharing` would.
    StartSharing { source: String },
    /// Stops presenting (or closes the picker); the camera comes back if it
    /// was on.
    StopSharing,
    /// How screen shares are sent, saved (`SETTING_SHARE_QUALITY`); a share
    /// in progress changes at once. Answered with `Event::CallDevices`.
    SetShareQuality(ShareQuality),
    /// Lists microphones, speakers and cameras; answered with `Event::CallDevices`.
    LoadCallDevices,
    /// Chooses a device for calls (`id` from `Event::CallDevices`), saved.
    SetCallDevice { kind: DeviceKind, id: String },
    /// Sends a reaction (an emoji) to everyone in the group call.
    CallReact { emoji: String },
    /// Raises (or lowers) our hand in the group call.
    RaiseHand(bool),
    /// Asks a participant's device to mute its microphone (Desktop's
    /// `sendRemoteMute`, behind the `remoteMute.send` flag; always on).
    RemoteMute { demux_id: u32 },
    /// The group call's participants on screen, whose video to receive
    /// (`main`: one shown large, at a higher resolution). Desktop's
    /// `setGroupCallVideoRequest`.
    ShowGroupVideo { visible: Vec<u32>, main: Option<u32> },
    /// Marks the call history read up to its latest call (`None`: every
    /// chat, as Desktop does when the Calls tab is viewed), and tells our
    /// other devices. Reading a chat does this for its calls.
    MarkCallsRead { conversation: Option<ConversationId> },
    /// Clears the call history (Desktop's "Clear call history"), here and
    /// on our other devices.
    ClearCallHistory,
    /// Desktop backups: asks for this device's recovery key (the account
    /// entropy pool the phone shared when linking). Answers with
    /// `Event::RecoveryKey`.
    LoadRecoveryKey,
    /// Desktop backups' "Choose folder": backups go in a `SignalBackups`
    /// folder made in `parent` ([`local_backup::SETTING_FOLDER`]).
    /// Answers with `Event::LocalBackupsChanged`.
    SetLocalBackupFolder { parent: PathBuf },
    /// The user typed the recovery key back (Desktop's "Confirm your
    /// recovery key", then "Keep your key safe"): its hash is kept in
    /// [`local_backup::SETTING_KEY_VIEWED`]. Answers with
    /// `Event::LocalBackupsChanged`.
    RecoveryKeyConfirmed,
    /// "Back up now": writes a backup to the folder, reporting
    /// `Event::LocalBackup` as it goes. One at a time.
    StartLocalBackup,
    /// Stops the backup in progress (it ends `Failed(Cancelled)`).
    CancelLocalBackup,
    /// "Turn off backups": forgets the folder, the last backup and the
    /// confirmed key; `delete` also removes the `SignalBackups` folder.
    DisableLocalBackups { delete: bool },
    /// Unlinks this device and wipes local state.
    Unlink,
    /// Stops the backend for good (the app quitting, an account removed
    /// from the app): no command is taken after it. A call in progress is
    /// hung up first, waiting up to two seconds for the hangup to be sent.
    /// Its thread ends with what it was doing.
    Stop,
    /// Stops waiting for the history transfer and continues without it.
    /// Ignored once the import has started.
    SkipHistory,
    /// The window became active: fetches the remote config again if it's
    /// been two hours (Desktop's `maybeRefreshRemoteConfig`).
    RefreshRemoteConfig,
    /// The machine woke from sleep: checks the server connections are still
    /// alive and reconnects if not (Desktop's `checkSockets` on
    /// `powerMonitorResume`, `ts/background.preload.ts`).
    Resumed,
    /// Someone's profile is being shown (their chat, About or contact
    /// details): fetched again unless it was in the last five minutes
    /// (Desktop's `throttledGetProfiles`). Answered with
    /// `Event::ProfileChanged` when anything shown changed.
    LoadProfile { service_id: String },
    /// Downloads a photo withheld because the chat isn't accepted (`d:`
    /// and a service id for a person's): Desktop's "View" on a
    /// placeholder (`startAvatarDownload`). Answered with
    /// `Event::ConversationsChanged` once it's here.
    DownloadPhoto { conversation: ConversationId },
    /// Writes our profile (Desktop's profile editor); answered with
    /// `Event::ProfileSaved`.
    SetProfile(profiles::ProfileEdit),
    /// Reserves a username (Desktop's username editor, after its half
    /// second pause): `discriminator` when digits were typed. Answered with
    /// `Event::UsernameReserved`; a newer one cancels an older one.
    ReserveUsername { nickname: String, discriminator: Option<String> },
    /// Makes a reservation ours; answered with `Event::UsernameSaved`.
    ConfirmUsername(usernames::Reservation),
    /// Deletes our username (and its link); answered with `Event::UsernameSaved`.
    DeleteUsername,
    /// Makes a new username link (the old one stops working); answered
    /// with `Event::UsernameLinkReset`.
    ResetUsernameLink,
    /// Sets our username link's QR code color (`UsernameLink.Color`).
    SetUsernameLinkColor(i32),
    /// The captcha from a `signalcaptcha://` link: sent to the server, and
    /// the messages a challenge held go again once it's accepted. Answered
    /// with `Event::Captcha`.
    SubmitCaptcha(String),
    /// Desktop's "Skip verification": the held messages fail.
    SkipCaptcha,
    /// A server alert closed: the idle warning (for a week), or the
    /// critical one's modal (for a day).
    DismissServerAlert(server_alerts::ServerAlert),
    /// Changes the chat folders (Desktop's chat folder settings and left
    /// pane), here and for the whole account (`ChatFolderRecord`s).
    /// Answered with `Event::ChatFoldersChanged`.
    EditChatFolders(langis_store::ChatFolderEdit),
    /// Changes the chat colors (Desktop's chat color settings): the
    /// default, one chat's, or the custom colors. Kept on this device, as
    /// Desktop keeps them. Answered with `Event::ChatColorsChanged` when
    /// something changed; read them with `AppDb::chat_colors`.
    EditChatColors(langis_store::ChatColorEdit),
    /// Changes the notification profiles or turns one on or off by hand
    /// (Desktop's Settings > Notifications > Notification profiles, and
    /// its chat list menu), here and for the whole account (each profile's
    /// `NotificationProfile` record, the override in the account record).
    /// New profiles take an id from `new_notification_profile_id`.
    /// Answered with `Event::NotificationProfilesChanged`.
    EditNotificationProfiles(langis_store::NotificationProfileEdit),
    /// An `Event::UnreadReminder` was shown: the next one for that chat waits
    /// three days and covers only messages after now.
    UnreadReminderShown { conversation: ConversationId },
}

/// A per-chat setting kept in the Storage Service.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatChange {
    Archived(bool),
    /// Unix millis; [`MUTED_ALWAYS`] for always, 0 unmutes.
    MutedUntil(i64),
    MarkedUnread(bool),
    Blocked(bool),
    /// Their stories hidden (Desktop's Hide story).
    HideStory(bool),
    /// A group's mentions while it's muted: `true` is Desktop's "Don't
    /// notify" (`dontNotifyForMentionsIfMuted`).
    DontNotifyMentions(bool),
    /// A person removed from our contacts (`true`, Desktop's `removeContact`)
    /// or restored. People only.
    Removed(bool),
    /// One of its "While muted" settings or its unread reminders
    /// (`langis_store::ChatNotify`); `None` follows the account setting
    /// ([`AccountSetting::for_chat`]). Mentions and replies are for groups.
    Notify(langis_store::ChatNotifyKind, Option<bool>),
}

/// A story list (Desktop's distribution list), from Storage Service.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct StoryList {
    /// Its UUID; My Story's is [`MY_STORY_ID`].
    pub id: String,
    pub name: String,
    /// Who it's shared with, or (`block_list`) who it isn't.
    pub members: Vec<String>,
    pub block_list: bool,
    pub allows_replies: bool,
}

impl StoryList {
    /// My Story as Desktop starts it: all Signal connections, replies on.
    pub fn my_story() -> Self {
        Self { id: MY_STORY_ID.into(), name: String::new(), members: Vec::new(), block_list: true, allows_replies: true }
    }

    pub fn is_my_story(&self) -> bool {
        self.id == MY_STORY_ID
    }

    /// A new custom story (Desktop's `createDistributionList`): replies on.
    pub fn new_custom(name: String, members: Vec<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name, members, block_list: false, allows_replies: true }
    }
}

/// Desktop's `MY_STORY_ID`.
pub const MY_STORY_ID: &str = "00000000-0000-0000-0000-000000000000";
/// Settings key: the story lists (JSON [`StoryList`]s), from Storage Service.
pub const SETTING_STORY_LISTS: &str = "story_lists";
/// Settings key: `"true"` once My Story's viewers were chosen (the
/// account's `hasSetMyStoriesPrivacy`); until then the first post asks.
pub const SETTING_MY_STORY_PRIVACY_SET: &str = "my_story_privacy_set";
/// Settings key: chats whose stories are hidden (JSON conversation ids).
pub const SETTING_HIDDEN_STORIES: &str = langis_store::HIDDEN_STORIES;

/// Whether the composer offers "Poll" in this chat (Desktop's
/// `CompositionArea`): groups, and 1:1 chats when the remote config's
/// `pollSend1to1` flag is on ([`RemoteConfig::poll_send_1to1`]). Received
/// polls show anywhere.
pub fn can_create_poll(kind: langis_store::ConversationKind, config: &RemoteConfig) -> bool {
    kind == langis_store::ConversationKind::Group || config.poll_send_1to1()
}

/// The history transfer after linking, as shown while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryProgress {
    /// Waiting for the phone to upload it.
    Waiting,
    /// Bytes downloaded, of `total` when the CDN says.
    Downloading { done: u64, total: Option<u64> },
    /// Bytes of the decrypted backup imported so far.
    Importing { done: u64, total: u64 },
    Done { chats: usize, messages: usize },
    /// The phone skipped it.
    Skipped,
    Failed(String),
}

/// "Muted always", as Signal's apps store it (Android's `Long.MAX_VALUE`).
pub const MUTED_ALWAYS: i64 = i64::MAX;

/// A page of GIF search results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GifResults {
    /// Each GIF with its downloaded preview.
    pub gifs: Vec<(giphy::Gif, std::path::PathBuf)>,
    pub next: Option<u32>,
}

/// Your Giphy key: Settings, then `GIPHY_API_KEY`.
pub(crate) async fn giphy_key(db: &AppDb) -> Option<String> {
    let clean = |key: String| Some(key.trim().to_owned()).filter(|k| !k.is_empty());
    // A cleared setting falls back to the environment.
    db.setting(SETTING_GIPHY_API_KEY)
        .await
        .ok()
        .flatten()
        .and_then(clean)
        .or_else(|| std::env::var("GIPHY_API_KEY").ok().and_then(clean))
}

/// Searches Giphy and downloads the result previews into `cache`.
pub(crate) async fn search_gifs(db: &AppDb, cache: &std::path::Path, query: &str, offset: u32) -> Result<GifResults, String> {
    let key = giphy_key(db).await.ok_or("Add a Giphy API key to search GIFs")?;
    let page = giphy::search(&key, query, offset).await.map_err(|e| format!("{e:#}"))?;
    std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let downloads = page.gifs.into_iter().map(|gif| async move {
        let path = cache.join(format!("{}.mp4", gif.id.replace(['/', '.'], "")));
        if !path.exists() {
            let bytes = giphy::download(&gif.preview.url).await.ok()?;
            // Whole files only: an interrupted write mustn't be reused.
            let partial = path.with_extension(format!("{}.part", rand::Rng::random::<u32>(&mut rand::rng())));
            tokio::fs::write(&partial, bytes).await.ok()?;
            tokio::fs::rename(&partial, &path).await.ok()?;
        }
        Some((gif, path))
    });
    let gifs = futures::future::join_all(downloads).await.into_iter().flatten().collect();
    Ok(GifResults { gifs, next: page.next })
}

/// An installed sticker pack, with its stickers saved to disk for display.
#[derive(Debug, Clone, PartialEq)]
pub struct StickerPackView {
    pub id: Vec<u8>,
    pub key: Vec<u8>,
    pub title: String,
    pub author: String,
    pub stickers: Vec<StickerView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickerView {
    pub id: u32,
    pub emoji: Option<String>,
    pub path: std::path::PathBuf,
}

/// A favorite sticker whose image we have, from an installed pack or not.
#[derive(Debug, Clone, PartialEq)]
pub struct FavoriteStickerView {
    pub pack_id: Vec<u8>,
    pub pack_key: Vec<u8>,
    pub sticker: StickerView,
}

/// A story to post.
#[derive(Debug, Clone, PartialEq)]
pub enum OutgoingStory {
    /// A text story, with the link preview it shows (Desktop's
    /// `TextAttachment.preview`; its image is uploaded with it).
    Text { story: Box<langis_store::TextStory>, preview: Option<LinkPreviewDraft> },
    /// A photo, or an MP4 video of up to 30 seconds, with a caption and its
    /// formatting (byte spans over the caption).
    Media { bytes: std::sync::Arc<Vec<u8>>, content_type: String, caption: String, styles: Vec<langis_store::StyleSpan> },
}

/// Where a story goes (Desktop's `SendStoryModal`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoryTarget {
    /// My Story: whoever its privacy says.
    MyStory,
    /// A custom story list, by id.
    List(String),
    /// A group's story, to its members.
    Group(ConversationId),
}

/// A view-once message's media, opened: a copy only the viewer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedViewOnce {
    pub message_id: i64,
    /// The copy; the viewer deletes it when it closes.
    pub path: PathBuf,
    pub content_type: String,
    /// Pixel size, when known.
    pub size: Option<(u32, u32)>,
}

/// A photo or video to send view-once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutgoingViewOnce {
    Image(OutgoingImage),
    /// A video, sent as it is.
    Video(OutgoingFile),
}

/// An image picked, pasted or dropped in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingImage {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub file_name: Option<String>,
}

/// The call in progress, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallUpdate {
    pub conversation: ConversationId,
    pub status: CallStatus,
    /// Started (or offered) as a video call.
    pub video: bool,
    /// They called us.
    pub incoming: bool,
    /// Their camera, and their microphone, are on.
    pub remote_video: bool,
    pub remote_audio: bool,
    /// We're presenting our screen; they're presenting theirs.
    pub sharing: bool,
    pub remote_sharing: bool,
    /// A group call; its participants (not us), in Desktop's order.
    pub group: bool,
    pub participants: Vec<CallParticipant>,
    /// Who's ringing us, for a group call's ring (their ACI).
    pub ringer: Option<String>,
    /// A group call's raised hands (demux ids, ours as `local_demux_id`),
    /// in the order they went up.
    pub raised_hands: Vec<u32>,
    /// Our demux id in a group call, once we're in.
    pub local_demux_id: Option<u32>,
    /// A call link's call: what's particular to one.
    pub link: Option<CallLinkCall>,
}

/// A reaction someone sent in a group call (Desktop's call reactions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallReaction {
    /// Their ACI (ours when `demux_id` is our `local_demux_id`).
    pub aci: String,
    pub demux_id: u32,
    pub emoji: String,
}

/// A call link's call, beyond what every group call has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallLinkCall {
    /// The link is ours: we let people in.
    pub admin: bool,
    /// Who's asking to join (their ACIs), in the order they asked.
    pub pending: Vec<String>,
    /// We're waiting for an admin to let us in.
    pub waiting: bool,
}

/// Someone in a group call (one of their devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallParticipant {
    /// Their ACI.
    pub aci: String,
    /// Their video's frames: `CallVideo::group.slot(demux_id)`.
    pub demux_id: u32,
    pub video: bool,
    pub audio: bool,
    pub presenting: bool,
    /// When they were last the main speaker, in Unix milliseconds (RingRTC's
    /// `speaker_time`). Desktop puts presenters first, then the most recent
    /// speaker, to choose who's large in speaker view.
    pub speaker_time: Option<i64>,
    /// We have their media keys (RingRTC's `media_keys_received`). Desktop
    /// shows "Can't receive audio and video from {name}" in their tile when
    /// they're still missing 5 seconds after the later of their `added_at`
    /// and our joining (`GroupCallRemoteParticipant`).
    pub media_keys: bool,
    /// When they joined the call, in Unix milliseconds.
    pub added_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStatus {
    /// They're calling; not answered yet.
    Ringing,
    /// We're calling: Desktop's "Connecting…", then "Ringing…".
    Calling,
    RingingThem,
    /// Answered; the connection is being set up (or recovering).
    Connecting,
    /// Since this time (unix millis).
    Connected { since: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Microphone,
    Speaker,
    Camera,
}

impl DeviceKind {
    /// Where the choice is saved.
    pub(crate) fn setting(self) -> &'static str {
        match self {
            Self::Microphone => "calls.microphone",
            Self::Speaker => "calls.speaker",
            Self::Camera => "calls.camera",
        }
    }
}

/// The devices calls can use; a chosen one is `None` for the system default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallDevices {
    pub devices: langis_calls::Devices,
    pub microphone: Option<String>,
    pub speaker: Option<String>,
    pub camera: Option<String>,
    /// How screen shares are sent (`Command::SetShareQuality`).
    pub share_quality: ShareQuality,
}

pub use langis_calls::{Device, Devices, PORTAL_SOURCE, ShareQuality, ShareSource};
/// A call link's URL from its root key (`https://signal.link/call/#key=…`).
pub use langis_calls::link_url as call_link_url;

/// The call's video: their latest frame, and our camera's (mirrored).
#[derive(Clone, Default)]
pub struct CallVideo {
    pub remote: langis_calls::FrameSlot,
    pub local: langis_calls::FrameSlot,
    /// A group call's participants' frames.
    pub group: langis_calls::GroupFrames,
    /// How loud each participant is, and we are (poll it with the frames).
    pub levels: langis_calls::AudioLevels,
}

pub use langis_calls::{Frame, FrameSlot};

/// A 1:1 contact's safety number, as Signal shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyNumber {
    /// 60 digits, shown in 12 groups of five.
    pub digits: String,
    /// The scannable fingerprint, for the QR code.
    pub qr: Vec<u8>,
    /// We marked their current identity key verified.
    pub verified: bool,
    /// Whether the screen offers automatic key verification.
    pub auto_verify: AutoVerify,
}

/// Desktop's key transparency section in the safety number screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVerify {
    /// Turned off (in Settings, or on another device): not shown.
    Off,
    /// We don't know their phone number (Desktop's `unavailable`).
    Unavailable,
    /// "Verify automatically" can be pressed.
    Available,
}

/// A file (not sent as an image) picked, pasted or dropped in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingFile {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub file_name: String,
    /// A recorded voice message (AAC in an M4A file), sent as one.
    pub voice: bool,
}

/// What a voice message is sent as: AAC (in an M4A file), as Signal's
/// phones send theirs.
pub const VOICE_CONTENT_TYPE: &str = "audio/aac";

/// Whether `len` bytes are over Signal's attachment size limit
/// (`global.attachments.maxBytes`, see [`RemoteConfig::attachment_max_bytes`]),
/// padded and encrypted as Desktop compares them (`isAttachmentTooLarge`).
pub fn too_big_to_send(len: usize, config: &RemoteConfig) -> bool {
    remote_config::too_large(len as u64, config.attachment_max_bytes())
}

/// A size limit as Desktop shows it (`getRenderDetailsForLimit` in
/// `ts/types/AttachmentSize.std.ts`): whole kB, MB or GB, in steps of 1024.
pub fn size_limit_label(bytes: u64) -> String {
    let units = ["kB", "MB", "GB"];
    let (mut limit, mut unit) = (bytes as f64, 0);
    limit /= 1024.;
    while limit >= 1024. && unit < units.len() - 1 {
        limit /= 1024.;
        unit += 1;
    }
    format!("{} {}", limit.trunc(), units[unit])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    /// Connected and draining the server queue.
    Syncing,
    Online,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// No account on disk; the UI should show the link screen.
    NeedsLinking,
    /// Provisioning URL to render as a QR code.
    LinkUrl(Url),
    /// The phone scanned the code and linked this device; the first sync
    /// (contacts, groups) runs until `Ready` (Desktop's "Syncing contacts
    /// and groups" step).
    Linked,
    /// Linking failed: what kind of failure (Desktop's install screen shows
    /// each its own way) and the error's text, for logs.
    LinkFailed { error: LinkError, detail: String },
    /// The phone linked another account than the one whose data is here.
    /// Nothing is used or deleted until `Command::ContinueLink` (Desktop's
    /// "Delete all data?" on its install screen).
    LinkedOtherAccount,
    /// Account is ready. Emitted at startup when already linked, and after linking.
    Ready { phone: Option<String>, aci: String },
    Connection(ConnectionState),
    /// Conversation list rows changed (title, preview, unread, ...).
    ConversationsChanged,
    /// Signal's megaphones to show now, highest priority first.
    Megaphones(Vec<megaphones::MegaphoneView>),
    /// Stories came, went, were viewed or replied to.
    StoriesChanged,
    /// A story's photo or video couldn't be downloaded (Desktop's "Can't
    /// download story"); a later `FetchStory` tries again.
    StoryDownloadFailed { id: i64 },
    /// Stories' settings changed (hidden stories, stories off, view
    /// receipts, story lists), here or from Storage Service.
    StorySettingsChanged,
    /// An [`AccountSetting`] changed, here or on another device.
    AccountSettingsChanged,
    /// A message was inserted or updated (status, reaction, edit, delete, attachment).
    MessageChanged { conversation: ConversationId, message_id: i64 },
    Typing { conversation: ConversationId, author: String, started: bool },
    /// A message is gone (deleted for me, here or on another device).
    MessageRemoved { conversation: ConversationId, message_id: i64 },
    /// A whole chat is gone.
    ConversationRemoved(ConversationId),
    /// Some of a chat's messages are gone (deleted on another device); reload it.
    ConversationCleared(ConversationId),
    /// A chat's pinned messages changed.
    PinsChanged(ConversationId),
    /// Who's in a group's call (their ACIs, one per device; empty: no call),
    /// and the most devices it takes, when the calling server says: with
    /// `users` at that many, the call is full (Desktop's "Call full").
    GroupPeek { conversation: ConversationId, users: Vec<String>, max_devices: Option<u32> },
    /// Screens and windows to present, or why there are none.
    ShareSources(Result<Vec<langis_calls::ShareSource>, String>),
    /// The devices calls can use, and which are chosen.
    CallDevices(CallDevices),
    /// Our camera couldn't start, or stopped on its own: why.
    CameraFailed(String),
    /// The call changed (`None`: no call any more).
    Call(Option<CallUpdate>),
    /// Reactions sent in the group call, oldest first.
    CallReactions(Vec<CallReaction>),
    /// In the group call, `by` (an ACI) muted `target` (`None`: us; then
    /// our microphone is off).
    CallMuted { by: String, target: Option<String> },
    /// Call links (or their calls in the history) changed: reload them.
    CallLinksChanged,
    /// A link we made is ready (its room id).
    CallLinkCreated(String),
    /// A link's URL was opened and the link is kept (its room id): its
    /// lobby can open.
    CallLinkOpened(String),
    /// A call link's call ended without us: an admin turned down our
    /// request, or removed us (Desktop's notices for both).
    CallLinkRefused { removed: bool },
    /// A 1:1 chat's safety number, or why there isn't one.
    SafetyNumber { conversation: ConversationId, result: Result<SafetyNumber, String> },
    /// Answer to `Command::CheckKeyTransparency`: whether their key (and
    /// ours) verified.
    KeyTransparency { conversation: ConversationId, verified: bool },
    /// Our own account's key transparency check has started failing:
    /// Desktop's "Automatic Key Verification is currently unavailable"
    /// dialog.
    KeyTransparencyFailed,
    /// A group we created is ready to open.
    GroupCreated(ConversationId),
    /// What a group link leads to, for Desktop's join dialog.
    GroupLinkPreview(GroupLinkPreview),
    /// Open this group (joined, asked to join, or already in it), with
    /// Desktop's toast if there is one ("You're already in this group.").
    GroupOpened { conversation: ConversationId, toast: Option<String> },
    /// A group link couldn't be read or joined: Desktop's title and words.
    GroupLinkFailed { title: String, description: String },
    /// Our answer to a message request took (for Desktop's toasts).
    MessageRequestAnswered { conversation: ConversationId, answer: langis_store::MessageRequestResponse },
    /// Progress of the message history transfer after linking.
    /// Draining the server queue after starting: the server timestamps
    /// (unix millis) of the first queued message and the latest one
    /// processed. Sent a few times a second at most, and only until the first
    /// `Connection(Online)`.
    CatchingUp { first: i64, latest: i64 },
    History(HistoryProgress),
    /// A view-once message we opened: show it in the viewer, without saving
    /// or forwarding, and delete the copy when it closes.
    ViewOnceOpened(OpenedViewOnce),
    /// The phone's contacts came (after linking, or "Import now").
    ContactsSynced,
    /// A new message from someone else arrived (for notifications): whether
    /// it mentions us or quotes one of ours (Desktop's `isMention`,
    /// `isReplyToMe`).
    Incoming { conversation: ConversationId, message_id: i64, mentions_me: bool, replies_to_me: bool },
    /// Someone else reacted to one of our messages (`emoji`), or took their
    /// reaction back (`None`), for Desktop's reaction notifications.
    /// `sender` is their name.
    Reacted { conversation: ConversationId, message_id: i64, author: String, sender: String, emoji: Option<String> },
    /// Someone voted in our poll (`ended`: they ended theirs), for
    /// Desktop's notifications. `message_id` is the poll; `sender` their name.
    PollActivity { conversation: ConversationId, message_id: i64, sender: String, ended: bool },
    /// GIF search results with their previews downloaded (`Err`: why not).
    Gifs { query: String, offset: u32, result: Result<GifResults, String> },
    /// Installed sticker packs (answer to `Command::LoadStickers`).
    Stickers(Vec<StickerPackView>),
    /// Favorite stickers, newest first (with `Event::Stickers`, and when
    /// they change).
    FavoriteStickers(Vec<FavoriteStickerView>),
    /// Every favorite sticker by (pack id, sticker id), including those
    /// whose image isn't here yet (Desktop's `getIsFavoriteStickerSelector`):
    /// at start, and after each `FavoriteStickers`.
    FavoriteStickerIds(Vec<(Vec<u8>, u32)>),
    /// How a `Command::UploadStickerPack` is going.
    StickerUpload(sticker_upload::StickerUpload),
    /// What the sticker manager lists (answer to `Command::LoadStickerManager`).
    StickerManager(stickers::StickerManagerView),
    /// `Command::SetStickerPackHidden` took (Desktop's "Sticker pack
    /// hidden" toast, with Undo, or "Sticker pack unhidden").
    StickerPackHidden { id: Vec<u8>, key: Vec<u8>, hidden: bool },
    /// A pack opened, or why it couldn't be (Desktop's "Error opening
    /// sticker pack").
    StickerPackPreview { id: Vec<u8>, result: Result<stickers::StickerPackPreview, String> },
    /// Answer to `Command::FindMessageStickerPack`: the pack's id and key,
    /// if the stored message names one.
    MessageStickerPack { message_id: i64, pack: Option<(Vec<u8>, Vec<u8>)> },
    /// Answer to `Command::FindRecipient`: who it is, or why not.
    RecipientFound { query: String, result: Result<langis_store::DirectoryEntry, String> },
    /// Answer to `Command::FetchLinkPreview` (`None`: no preview for it).
    LinkPreview { url: String, preview: Option<LinkPreviewDraft> },
    /// An attachment downloading: bytes received of `total` (the sender's
    /// size, 0 if unknown), a few times a second. `MessageChanged` follows
    /// when it's done; a cancelled or failed one ends with `received` 0.
    DownloadProgress { message_id: i64, index: usize, received: u64, total: u64 },
    /// An attachment couldn't be downloaded: `permanent` when it's gone
    /// (Desktop's "This media is not available"), otherwise it can be
    /// tried again with `FetchAttachment`.
    AttachmentUnavailable { message_id: i64, index: usize, permanent: bool },
    /// Stored author names changed: `(service id, name)` pairs. Loaded
    /// messages and quotes by these authors should take the new names.
    AuthorNamesChanged(Vec<(String, String)>),
    /// The remote config changed (see [`remote_config()`]): limits and
    /// features may have too.
    RemoteConfigChanged,
    /// Someone's profile (name, about, badges) or contact details changed.
    ProfileChanged(String),
    /// Our profile, username or username link changed: read [`own_profile`]
    /// again.
    OwnProfileChanged,
    /// Badge descriptions or images arrived or changed.
    BadgesChanged,
    /// Answer to `Command::SetProfile`: Desktop's "Your profile could not
    /// be updated" on failure.
    ProfileSaved(Result<(), String>),
    /// Answer to `Command::ReserveUsername`, for that nickname and digits.
    UsernameReserved {
        nickname: String,
        discriminator: Option<String>,
        result: Result<usernames::Reservation, usernames::UsernameProblem>,
    },
    /// Answer to `Command::ConfirmUsername` (the username now ours) and
    /// `Command::DeleteUsername` (`None`).
    UsernameSaved(Result<Option<String>, usernames::UsernameProblem>),
    /// Answer to `Command::ResetUsernameLink`.
    UsernameLinkReset(Result<(), String>),
    /// A send needs a captcha first (HTTP 428): Desktop's captcha dialog.
    CaptchaRequired,
    /// Answer to `Command::SubmitCaptcha`: accepted, or why not.
    Captcha(Result<(), String>),
    /// The server's alerts as last connected (Desktop's `serverAlerts`).
    ServerAlerts(server_alerts::ServerAlerts),
    /// The critical idle-phone alert's modal is due.
    CriticalAlertModal,
    /// Our clock is too far from the server's (`true`), or not any more.
    ClockSkew(bool),
    /// The chat folders changed, here or on another device: read them again.
    ChatFoldersChanged,
    /// The chat colors changed: read them again.
    ChatColorsChanged,
    /// The notification profiles or the override changed, here or on
    /// another device: read `AppDb::notification_profile_state` again.
    NotificationProfilesChanged,
    /// Desktop's unread reminder: a muted chat has had unread messages for
    /// days. Mention and reply counts are zeroed when that kind is off
    /// there while muted. Show it as a notification and answer
    /// `Command::UnreadReminderShown`; until then it comes again hourly.
    UnreadReminder { conversation: ConversationId, reminder: langis_store::ReminderData },
    /// Ask "How was your call?" about a call that just ended (Desktop's
    /// `shouldShowCallQualitySurvey`); the answers go back with
    /// `Command::SubmitCallQualitySurvey`.
    CallQualitySurvey(call_survey::CallSurvey),
    /// Settings > Backups (answer to `Command::RefreshBackupStatus`).
    BackupStatus(backups::BackupStatus),
    /// How the download of a transferred backup's media is going: at
    /// start, a few times a second while it moves, and after each
    /// `Command::BackupMediaDownload`.
    BackupMediaDownload(backups::BackupMediaDownload),
    /// The answer to `Command::LoadRecoveryKey`: the 64-character recovery
    /// key, or `None` when this device wasn't given one (show it with
    /// [`local_backup::format_key`]).
    RecoveryKey(Option<String>),
    /// How a Desktop backup is going (`Command::StartLocalBackup`).
    LocalBackup(local_backup::LocalBackupProgress),
    /// Desktop backups' settings changed (folder, last backup, confirmed
    /// key): read the `local_backup::SETTING_*` keys again.
    LocalBackupsChanged,
    /// `Command::SubmitCallQualitySurvey` went (Desktop's "Thanks for your
    /// feedback!") or failed (`Err`: why; Desktop's "Failed to submit
    /// survey").
    CallQualitySurveySent(Result<(), String>),
    Error(String),
    /// This device is no longer linked. After `Command::Unlink` the local
    /// account and history are gone. When the phone removed the device
    /// (the server refuses our credentials), only the registration and
    /// protocol state go, as Desktop's `unlinkAndDisconnect`: linking the
    /// same account again keeps the chats. `NeedsLinking` follows.
    Unlinked,
}

/// Our profile as the app shows it (Desktop's own conversation and items).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OwnProfile {
    pub given_name: String,
    pub family_name: String,
    pub about: String,
    pub about_emoji: String,
    /// Our downloaded profile photo.
    pub photo: Option<PathBuf>,
    pub username: Option<String>,
    pub username_link: Option<usernames::UsernameLink>,
    /// Our profile's badges, visible or not.
    pub badges: Vec<langis_store::ProfileBadge>,
    /// The server no longer has our username, or our link (Desktop's
    /// `usernameCorrupted`, `usernameLinkCorrupted`): its left-pane banner
    /// offers to set it again.
    pub username_corrupted: bool,
    pub username_link_corrupted: bool,
}

impl OwnProfile {
    /// Our profile name, given and family name combined.
    pub fn name(&self) -> Option<String> {
        langis_store::combine_names(&self.given_name, &self.family_name)
    }
}

/// Reads our profile from the database (`aci`: ours).
pub async fn own_profile(db: &AppDb, aci: &str) -> OwnProfile {
    let profile = db.person_profile(aci).await.ok().flatten().unwrap_or_default();
    let photo = db
        .person_avatars()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|(sid, _)| sid == aci)
        .map(|(_, path)| PathBuf::from(path))
        .filter(|p| p.exists());
    let username = db.setting(key_transparency::SETTING_USERNAME).await.ok().flatten().filter(|u| !u.is_empty());
    let username_link = db
        .setting(usernames::SETTING_USERNAME_LINK)
        .await
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .filter(|_| username.is_some());
    let flag = |v: Option<String>| v.is_some_and(|v| v == "1");
    let username_corrupted = username.is_some() && flag(db.setting(usernames::SETTING_USERNAME_CORRUPTED).await.ok().flatten());
    let username_link_corrupted =
        username_link.is_some() && flag(db.setting(usernames::SETTING_USERNAME_LINK_CORRUPTED).await.ok().flatten());
    OwnProfile {
        username_corrupted,
        username_link_corrupted,
        given_name: profile.given_name,
        family_name: profile.family_name,
        about: profile.about,
        about_emoji: profile.about_emoji,
        photo,
        username,
        username_link,
        badges: profile.badges,
    }
}

/// What the About and contact screens show about someone (Desktop's
/// `AboutContactModal` and `ContactModal` props).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContactDetails {
    /// Their profile and contact record details, when known.
    pub profile: Option<langis_store::PersonProfile>,
    /// They're in our address book (Desktop's `isInSystemContacts`).
    pub in_system_contacts: bool,
    /// The nickname and note we gave them.
    pub nickname: Option<langis_store::ContactNickname>,
    /// We marked their current identity key verified (Desktop's
    /// `isVerified`): the key we verified is the one we last saw.
    pub verified: bool,
    /// We share our profile with them (Desktop's `profileSharing`).
    pub profile_sharing: bool,
}

/// Reads what the About and contact screens show about `service_id`.
pub async fn contact_details(db: &AppDb, service_id: &str) -> ContactDetails {
    let verified_key = db.setting(&format!("{}{service_id}", actor::SETTING_VERIFIED)).await.ok().flatten().filter(|k| !k.is_empty());
    let current_key = db.setting(&format!("{}{service_id}", actor::SETTING_IDENTITY)).await.ok().flatten();
    ContactDetails {
        profile: db.person_profile(service_id).await.ok().flatten(),
        in_system_contacts: db.system_contacts().await.unwrap_or_default().iter().any(|s| s == service_id),
        nickname: db.contact_nickname(service_id).await.ok().flatten(),
        // No key recorded yet: the verified one is the one we have.
        verified: verified_key.is_some() && (current_key.is_none() || verified_key == current_key),
        profile_sharing: db.profile_sharing(&ConversationId::direct(service_id)).await.unwrap_or(false),
    }
}

/// Who we marked verified and whose identity key has changed since
/// (Desktop's `isUnverified`, the `UNVERIFIED` state): sends to them wait
/// for Desktop's "Safety Number Changes" confirmation
/// (`blockSendUntilConversationsAreVerified`).
pub async fn unverified_people(db: &AppDb) -> std::collections::HashSet<String> {
    let verified = db.settings_with_prefix(actor::SETTING_VERIFIED).await.unwrap_or_default();
    let current: std::collections::HashMap<String, String> =
        db.settings_with_prefix(actor::SETTING_IDENTITY).await.unwrap_or_default().into_iter().collect();
    // A send refused for their new key (the store holds it until the user
    // answers) counts as changed too.
    let pending: std::collections::HashMap<String, String> =
        db.settings_with_prefix(actor::SETTING_IDENTITY_PENDING).await.unwrap_or_default().into_iter().collect();
    let changed = |sid: &String, key: &String| {
        current.get(sid).is_some_and(|now| now != key) || pending.get(sid).is_some_and(|new| !new.is_empty() && new != key)
    };
    verified.into_iter().filter(|(sid, key)| !key.is_empty() && changed(sid, key)).map(|(sid, _)| sid).collect()
}

/// Of the people a send to `conversation` goes to, those waiting for the
/// "Safety Number Changes" confirmation (see [`unverified_people`]).
pub fn stopping_send(conversation: &langis_store::Conversation, unverified: &std::collections::HashSet<String>) -> Vec<String> {
    match conversation.kind {
        langis_store::ConversationKind::Direct => conversation.id.as_direct().filter(|sid| unverified.contains(*sid)).map(str::to_owned).into_iter().collect(),
        langis_store::ConversationKind::Group => conversation.members.iter().filter(|m| unverified.contains(*m)).cloned().collect(),
        langis_store::ConversationKind::NoteToSelf => Vec::new(),
    }
}

/// The backend thread's stack (see `Backend::start`).
const BACKEND_STACK: usize = 16 * 1024 * 1024;

/// Handle to the running backend. Cheap to clone.
#[derive(Clone)]
pub struct Backend {
    db: AppDb,
    video: CallVideo,
    commands: mpsc::UnboundedSender<Command>,
    runtime: tokio::runtime::Handle,
    data_dir: std::path::PathBuf,
    remote_config: remote_config::ConfigCell,
    /// Set once the backend thread is done (see [`Backend::stop`]).
    finished: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

/// Takes `data_dir/langis.lock` for this process, or fails saying the
/// directory is in use.
fn lock_data_dir(data_dir: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let path = data_dir.join("langis.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("{} is already in use by another running copy", data_dir.display())
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error).with_context(|| format!("locking {}", path.display())),
    }
}

impl Backend {
    /// Where the databases, attachments and logs live.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// This account's remote config in effect.
    pub fn remote_config(&self) -> std::sync::Arc<RemoteConfig> {
        self.remote_config.get()
    }

    /// Opens the databases, starts the runtime thread, and begins connecting
    /// (or emits `NeedsLinking`). Events arrive on the returned receiver, which
    /// may be polled from any executor.
    pub fn start(config: Config) -> anyhow::Result<(Backend, mpsc::UnboundedReceiver<Event>)> {
        std::fs::create_dir_all(config.data_dir.join("attachments"))
            .with_context(|| format!("creating {}", config.data_dir.display()))?;
        // Attachments, avatars, stickers and logs are stored decrypted under
        // the data directory: only its owner may enter it, whatever the umask.
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making {} private", config.data_dir.display()))?;
        }
        // One backend per data directory: a second would connect as the same
        // device and write the same databases. Held until the backend's
        // thread ends (the system lets go of it if the process dies).
        let lock = lock_data_dir(&config.data_dir)?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("signal-worker")
            .enable_all()
            .build()
            .context("starting tokio runtime")?;

        let db = runtime
            .block_on(AppDb::open(&config.data_dir.join("app.db"), &config.db_key))
            .context("opening app database")?;
        if let Err(error) = runtime.block_on(db.upgrade_colors()) {
            tracing::warn!(%error, "failed to recompute avatar colors");
        }
        if let Err(error) = runtime.block_on(db.upgrade_previews()) {
            tracing::warn!(%error, "failed to refresh conversation previews");
        }
        for former in &config.former_data_dirs {
            runtime.block_on(rebase_paths(&db, former, &config.data_dir));
        }

        // Never let the pool drop implicitly: see `actor::run` on SQLCipher's
        // exit-time shutdown. `Backend::shutdown` closes it explicitly.
        std::mem::forget(db.clone());

        let (commands, command_rx) = mpsc::unbounded();
        let (event_tx, events) = mpsc::unbounded();
        let video = CallVideo::default();
        let backend = Backend {
            db: db.clone(),
            video: video.clone(),
            commands,
            runtime: runtime.handle().clone(),
            data_dir: config.data_dir.clone(),
            remote_config: remote_config::ConfigCell::default(),
            finished: Default::default(),
        };
        let config_cell = backend.remote_config.clone();
        let finished = backend.finished.clone();

        std::thread::Builder::new()
            .name("langis-core".into())
            // Decrypting a message runs deep, generic futures (presage's and
            // libsignal-service's, built in this crate); unoptimized in debug
            // builds, they overflowed the default 2 MB stack.
            .stack_size(BACKEND_STACK)
            .spawn(move || {
                // presage's futures are !Send, so the actor lives on a LocalSet
                // pinned to this thread; `Backend::spawn` work goes to the workers.
                let local = tokio::task::LocalSet::new();
                remote_config::install(config_cell);
                if config.demo {
                    runtime.block_on(local.run_until(demo::run(db, config.data_dir.clone(), command_rx, event_tx)));
                } else {
                    runtime.block_on(local.run_until(actor::run(config, db, video, command_rx, event_tx)));
                }
                // Its tasks go with it.
                drop(local);
                // Free before `stop` returns, so the directory can be opened again.
                drop(lock);
                let (done, ended) = &*finished;
                *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
                ended.notify_all();
                // `Backend::spawn` and `shutdown` use this runtime's handle
                // even if the actor stopped early (signal.db failed to open,
                // say): never shut it down.
                std::mem::forget(runtime);
            })
            .context("spawning backend thread")?;

        Ok((backend, events))
    }

    /// Closes the app database (checkpointing its WAL) before the process
    /// exits. Blocks for at most two seconds; call from a non-runtime thread.
    pub fn shutdown(&self) {
        let db = self.db.clone();
        let _ = self
            .runtime
            .block_on(async move { tokio::time::timeout(std::time::Duration::from_secs(2), db.close()).await });
    }

    pub fn db(&self) -> &AppDb {
        &self.db
    }

    /// The call's video frames.
    pub fn call_video(&self) -> &CallVideo {
        &self.video
    }

    /// Stops the backend (see [`Command::Stop`], taken after the commands
    /// before it), waits up to `wait` for its thread to finish what it was
    /// doing (so nothing writes in its data directory after), and closes
    /// its database. Returns whether the thread finished. Call from a
    /// non-runtime thread.
    pub fn stop(&self, wait: std::time::Duration) -> bool {
        self.send(Command::Stop);
        let (done, ended) = &*self.finished;
        let guard = done.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = ended
            .wait_timeout_while(guard, wait, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        let finished = *guard;
        drop(guard);
        self.shutdown();
        finished
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.unbounded_send(command);
    }

    /// Runs `fut` on the backend runtime (needed for `AppDb` queries) and
    /// returns a future that can be awaited from any executor.
    pub fn spawn<F>(&self, fut: F) -> impl Future<Output = F::Output> + Send + 'static
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.runtime.spawn(fut);
        async move { handle.await.expect("backend task panicked") }
    }
}

/// Rewrites the paths stored for files in `former`, the data directory
/// before it moved to `data_dir`, before anything reads them: once per
/// former directory. A failure is tried again at the next start.
async fn rebase_paths(db: &AppDb, former: &std::path::Path, data_dir: &std::path::Path) {
    let (from, to) = (former.to_string_lossy(), data_dir.to_string_lossy());
    let done = format!("{}:{from}", langis_store::SETTING_REBASED_FROM);
    // (The single setting before there were two former names held the one
    // directory done.)
    let earlier = db.setting(langis_store::SETTING_REBASED_FROM).await.ok().flatten();
    if from == to || earlier.as_deref() == Some(&*from) || db.setting(&done).await.ok().flatten().is_some() {
        return;
    }
    let started = std::time::Instant::now();
    match db.rebase_paths(&from, &to).await {
        Ok(changed) => {
            tracing::info!(changed, elapsed = ?started.elapsed(), "rewrote stored file paths from {from}");
            if let Err(error) = db.set_setting(&done, "1").await {
                tracing::warn!(%error, "couldn't record the rewritten file paths");
            }
        }
        Err(error) => tracing::warn!(%error, "couldn't rewrite stored file paths from {from}"),
    }
}

#[cfg(test)]
mod call_line_tests {
    use super::CallLines;

    #[test]
    fn an_account_is_busy_while_another_is_in_a_call() {
        let lines = CallLines::default();
        let (a, b) = (std::path::Path::new("/a"), std::path::Path::new("/b"));
        assert!(!lines.busy_elsewhere(a) && !lines.busy_elsewhere(b));
        lines.set(a, true);
        assert!(lines.busy_elsewhere(b), "a's call");
        assert!(!lines.busy_elsewhere(a), "its own call is RingRTC's to handle");
        lines.set(a, false);
        assert!(!lines.busy_elsewhere(b));
    }
}

#[cfg(test)]
mod verification_tests {
    use super::{actor, unverified_people};

    #[tokio::test]
    async fn a_refused_new_key_holds_sends_until_the_user_answers() {
        let db = langis_store::AppDb::open_in_memory().await.unwrap();
        let (alice, bob) = ("alice-aci", "bob-aci");
        for (sid, key) in [(alice, "aa"), (bob, "bb")] {
            db.set_setting(&format!("{}{sid}", actor::SETTING_VERIFIED), key).await.unwrap();
            db.set_setting(&format!("{}{sid}", actor::SETTING_IDENTITY), key).await.unwrap();
        }
        assert!(unverified_people(&db).await.is_empty());

        // A send to Alice was refused for her new key.
        db.set_setting(&format!("{}{alice}", actor::SETTING_IDENTITY_PENDING), "cc").await.unwrap();
        assert_eq!(unverified_people(&db).await, [alice.to_owned()].into());

        // Seen on receive instead: the same.
        db.set_setting(&format!("{}{bob}", actor::SETTING_IDENTITY), "dd").await.unwrap();
        assert_eq!(unverified_people(&db).await.len(), 2);

        // The user answered for Alice ("send anyway": no longer verified).
        db.remove_setting(&format!("{}{alice}", actor::SETTING_IDENTITY_PENDING)).await.unwrap();
        db.set_setting(&format!("{}{alice}", actor::SETTING_VERIFIED), "").await.unwrap();
        assert_eq!(unverified_people(&db).await, [bob.to_owned()].into());
    }
}
