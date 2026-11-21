//! Signal Secure Backups as a linked Desktop shows them (Settings > Backups,
//! `ts/components/PreferencesBackups.dom.tsx` and
//! `PreferencesBackupDetails.dom.tsx`), and the download of a transferred
//! backup's media (`ts/util/backupMediaDownload.preload.ts`, the left
//! pane's `BackupMediaDownloadProgress`).
//!
//! Only the phone sets backups up or pays for them. What a linked device
//! shows comes from three places, as in Desktop:
//!
//! - the plan: the account record's `backupTier` (Storage Service), unset
//!   when backups are off;
//! - the subscription (paid plans): `GET /v1/subscription/{subscriberId}`,
//!   unauthenticated, with the subscriber id from the account record's
//!   `backupSubscriberData` or the transferred backup's account data
//!   (`BackupAPI.getSubscriptionInfo`);
//! - the last backup: the message backup's file on the backup CDN, its
//!   `Last-Modified` and `Content-Length` (`BackupAPI.getBackupProtoInfo`),
//!   read with a message backup credential, which is keyed by the backup
//!   key from the account entropy pool.

use std::cell::Cell;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use langis_store::AppDb;
use libsignal_account_keys::BackupKey;
use libsignal_core::Aci;
use presage::libsignal_service::configuration::Endpoint;
use presage::libsignal_service::push_service::HttpAuthOverride;
use presage::store::StateStore as _;
use zkgroup::backups::BackupLevel;

use crate::backup_media::{self, CredentialKind};
use crate::{Command, Event};

/// Settings key: the backup subscriber id (hex), from the account record's
/// `backupSubscriberData` or the transferred backup (Desktop's
/// `backupsSubscriberId`). Empty or unset: none.
pub(crate) const SETTING_SUBSCRIBER_ID: &str = "backup.subscriber_id";
/// Settings key: the last subscription status fetched, as JSON
/// ([`Subscription`] and when; Desktop's `backupSubscriptionStatus`).
const SETTING_SUBSCRIPTION: &str = "backup.subscription";
/// Settings key: the last backup's details fetched, as JSON (Desktop's
/// `cloudBackupStatus`); `null` when there's no backup.
const SETTING_CLOUD_BACKUP: &str = "backup.cloud";
/// Settings key: the message backup signing key's public half (base64)
/// last uploaded (Desktop's `setBackupMessagesSignatureKey`).
const SETTING_MESSAGES_KEY_SET: &str = "backup.messages_signature_key_set";
/// Settings key: `"true"` while the backup media download is paused
/// (Desktop's `backupMediaDownloadPaused`).
pub(crate) const SETTING_DOWNLOAD_PAUSED: &str = "backup.media_download.paused";
/// Settings key: `"true"` once the left pane's banner was closed (Desktop's
/// `backupMediaDownloadBannerDismissed`).
pub(crate) const SETTING_DOWNLOAD_DISMISSED: &str = "backup.media_download.dismissed";

/// Desktop throttles both fetches to one every 30 seconds
/// (`throttledFetchSubscriptionStatus`, `throttledFetchCloudBackupStatus`).
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Desktop's `SUBSCRIPTION_STATUS_STALE_TIME_FOR_UI`.
const STALE_FOR_SPINNER_MS: i64 = 5 * 60 * 1000;
/// Desktop's `MONTH` in `getMessageQueueTime`.
const MONTH_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Desktop's `ZERO_DECIMAL_CURRENCIES` (`ts/util/currency.dom.ts`): the
/// server gives other currencies' amounts in hundredths.
const ZERO_DECIMAL_CURRENCIES: [&str; 15] =
    ["bif", "clp", "djf", "gnf", "jpy", "kmf", "krw", "mga", "pyg", "rwf", "vnd", "vuv", "xaf", "xof", "xpf"];

/// The account's backup plan (the account record's `backupTier`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BackupTier {
    /// Text and the last `free_media_days` of media.
    Free,
    /// Text and all media, by subscription.
    Paid,
}

impl BackupTier {
    /// From the stored zkgroup `BackupLevel`; `None` when backups are off
    /// or the record hasn't been read.
    fn from_setting(value: Option<&str>) -> Option<Self> {
        match value?.parse::<u64>().ok()? {
            l if l == u64::from(BackupLevel::Free) => Some(BackupTier::Free),
            l if l == u64::from(BackupLevel::Paid) => Some(BackupTier::Paid),
            _ => None,
        }
    }
}

/// A price: `amount` in the currency's whole units (2.99, not 299).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cost {
    pub amount: f64,
    /// ISO 4217, upper case as the server gives it (`USD`).
    pub currency: String,
}

/// Desktop's `BackupsSubscriptionType`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Subscription {
    /// Paid and renewing: "{cost} / month", "Renews {date}".
    Active { cost: Option<Cost>, renews_at: Option<i64> },
    /// Canceled, paid until `expires_at`: "Subscription canceled",
    /// "Expires {date}".
    PendingCancellation { cost: Option<Cost>, expires_at: Option<i64> },
    /// No subscriber id, or the server has no subscription for it (also
    /// what a failed fetch gives, as in Desktop).
    NotFound,
    /// The subscription is no longer active.
    Expired,
}

/// The last backup the phone made (Desktop's `BackupStatusType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CloudBackup {
    /// When it was made (Unix millis; the file's `Last-Modified`).
    pub created_at: i64,
    /// Its size in bytes (Desktop fetches it but doesn't show it).
    pub size: u64,
}

/// What Settings > Backups shows, in `Event::BackupStatus`.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupStatus {
    /// `None`: backups are off, or not set up (Desktop's "Signal Secure
    /// Backups" row with "Get started on your phone").
    pub tier: Option<BackupTier>,
    /// For a free plan: how many days of media it keeps (Desktop's
    /// `backupFreeMediaDays`, the remote config's message queue time).
    pub free_media_days: u32,
    /// The paid plan's subscription as last fetched; `None` before the
    /// first fetch (Desktop shows that as not found).
    pub subscription: Option<Subscription>,
    /// When `subscription` was fetched (Unix millis).
    pub subscription_fetched_at: Option<i64>,
    /// A fetch is under way; another `Event::BackupStatus` follows.
    pub fetching: bool,
    /// The last backup, once fetched (details page only); `None` also when
    /// there's no backup.
    pub last_backup: Option<CloudBackup>,
}

impl BackupStatus {
    /// Desktop's spinner in place of the subscription details: fetching,
    /// and what's shown is over five minutes old.
    pub fn show_spinner(&self, now_ms: i64) -> bool {
        self.fetching && now_ms - self.subscription_fetched_at.unwrap_or(0) > STALE_FOR_SPINNER_MS
    }
}

/// How the download of a transferred backup's media is going, in
/// `Event::BackupMediaDownload` (Desktop's `getBackupMediaDownloadProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackupMediaDownload {
    /// Whether the account has paid media backups (Desktop's
    /// `isBackupMediaEnabled`): the banner shows only then.
    pub media_backups: bool,
    /// Bytes of everything queued since the counts were last reset, and of
    /// what's finished (downloaded or given up). Equal: done. Zero: nothing
    /// to show.
    pub total_bytes: u64,
    pub completed_bytes: u64,
    /// Paused by `BackupMediaDownloadAction::Pause`.
    pub paused: bool,
    /// Nothing can download now (offline, or the backup tier failing) and
    /// it'll go on later: Desktop's "{n} of {total} restored", "Media will
    /// continue to download in the background".
    pub idle: bool,
    /// The left pane's banner was closed (`BackupMediaDownloadAction::Dismiss`).
    pub dismissed: bool,
}

impl BackupMediaDownload {
    /// Desktop's `showBackupMediaDownloadProgress` and
    /// `BackupMediaDownloadProgress`'s `totalBytes <= 0` check.
    pub fn show_banner(&self) -> bool {
        self.media_backups && !self.dismissed && self.total_bytes > 0
    }

    pub fn complete(&self) -> bool {
        self.total_bytes > 0 && self.completed_bytes >= self.total_bytes
    }
}

/// `Command::BackupMediaDownload`: Desktop's banner and settings buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupMediaDownloadAction {
    /// Stop after the file in hand; nothing more downloads until resumed,
    /// across restarts too.
    Pause,
    /// Go on, retrying what was waiting at once.
    Resume,
    /// Drop everything still queued and reset the counts (after Desktop's
    /// "Cancel media syncing?" confirmation). Those files stay
    /// downloadable one by one.
    Cancel,
    /// Close the left pane's banner (shown with the download complete or
    /// idle).
    Dismiss,
}

// ------------------------------------------------------------------ parsing

/// `GET /v1/subscription/{id}`: the id in base64url without padding
/// (Desktop's `toWebSafeBase64`).
pub(crate) fn subscription_path(subscriber_id: &[u8]) -> String {
    format!("/v1/subscription/{}", URL_SAFE_NO_PAD.encode(subscriber_id))
}

/// Reads `GET /v1/subscription`'s answer as Desktop's
/// `getSubscriptionInfo` does.
pub(crate) fn parse_subscription(json: &[u8]) -> anyhow::Result<Subscription> {
    #[derive(serde::Deserialize)]
    struct Answer {
        subscription: Option<Details>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Details {
        #[serde(rename = "endOfCurrentPeriod")]
        end: Option<f64>,
        active: bool,
        #[serde(default)]
        cancel_at_period_end: bool,
        currency: Option<String>,
        amount: Option<f64>,
    }
    let answer: Answer = serde_json::from_slice(json).context("unexpected subscription answer")?;
    let Some(details) = answer.subscription else { return Ok(Subscription::NotFound) };
    if !details.active {
        return Ok(Subscription::Expired);
    }
    let cost = match (details.amount, details.currency) {
        (Some(amount), Some(currency)) if amount > 0. && !currency.is_empty() => {
            let whole = ZERO_DECIMAL_CURRENCIES.contains(&currency.to_lowercase().as_str());
            Some(Cost { amount: if whole { amount } else { amount / 100. }, currency })
        }
        _ => None,
    };
    let end = details.end.map(|seconds| (seconds * 1000.) as i64);
    Ok(if details.cancel_at_period_end {
        Subscription::PendingCancellation { cost, expires_at: end }
    } else {
        Subscription::Active { cost, renews_at: end }
    })
}

/// `GET /v1/archives` with a message backup credential:
/// `(cdn, backupDir, backupName)` (Desktop's `getMessageBackupInfo`).
pub(crate) fn parse_message_backup_info(json: &[u8]) -> anyhow::Result<(u32, String, String)> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Answer {
        cdn: u32,
        backup_dir: String,
        backup_name: String,
    }
    let answer: Answer = serde_json::from_slice(json).context("unexpected backup info")?;
    Ok((answer.cdn, answer.backup_dir, answer.backup_name))
}

/// Where the message backup is on its CDN (Desktop's
/// `getBackupFileHeaders`: `/backups/{backupDir}/{backupName}`). The
/// server's names are base64url; anything else is refused.
pub(crate) fn backup_file_path(backup_dir: &str, backup_name: &str) -> anyhow::Result<String> {
    let safe = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b));
    if !safe(backup_dir) || !safe(backup_name) {
        bail!("unexpected backup file names");
    }
    Ok(format!("backups/{backup_dir}/{backup_name}"))
}

/// The backup file's `Last-Modified` and `Content-Length`.
pub(crate) fn cloud_backup_of(last_modified: &str, length: &str) -> anyhow::Result<CloudBackup> {
    let created = chrono::DateTime::parse_from_rfc2822(last_modified.trim()).context("unexpected Last-Modified")?;
    let size = length.trim().parse::<u64>().context("unexpected Content-Length")?;
    Ok(CloudBackup { created_at: created.timestamp_millis(), size })
}

/// Desktop's `backupFreeMediaDays`: `getMessageQueueTime` in days, the
/// remote config's `global.messageQueueTimeInSeconds` but at least a month.
pub(crate) fn free_media_days(config: &crate::RemoteConfig) -> u32 {
    let seconds = config.int("global.messageQueueTimeInSeconds").unwrap_or(MONTH_SECONDS).max(MONTH_SECONDS);
    (seconds / (24 * 60 * 60)) as u32
}

// -------------------------------------------------------------------- state

#[derive(serde::Serialize, serde::Deserialize)]
struct Fetched<T> {
    value: T,
    at: i64,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

async fn read_json<T: serde::de::DeserializeOwned>(db: &AppDb, key: &str) -> Option<T> {
    db.setting(key).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok())
}

/// The status as last fetched.
pub(crate) async fn stored_status(db: &AppDb, fetching: bool) -> BackupStatus {
    let tier = BackupTier::from_setting(db.setting(backup_media::SETTING_BACKUP_TIER).await.ok().flatten().as_deref());
    let subscription: Option<Fetched<Subscription>> = read_json(db, SETTING_SUBSCRIPTION).await;
    let last_backup: Option<Fetched<Option<CloudBackup>>> = read_json(db, SETTING_CLOUD_BACKUP).await;
    BackupStatus {
        tier,
        free_media_days: free_media_days(&crate::remote_config()),
        subscription_fetched_at: subscription.as_ref().map(|s| s.at),
        subscription: subscription.map(|s| s.value),
        fetching,
        last_backup: last_backup.and_then(|b| b.value),
    }
}

/// The backup media download's state (`idle` is the actor's to say).
pub(crate) async fn download_status(db: &AppDb, idle: bool) -> BackupMediaDownload {
    let on = |v: Option<String>| v.as_deref() == Some("true");
    let progress = db.backup_media_progress().await.unwrap_or_default();
    let tier = db.setting(backup_media::SETTING_BACKUP_TIER).await.ok().flatten();
    let paused = on(db.setting(SETTING_DOWNLOAD_PAUSED).await.ok().flatten());
    BackupMediaDownload {
        media_backups: BackupTier::from_setting(tier.as_deref()) == Some(BackupTier::Paid),
        total_bytes: progress.total_bytes,
        completed_bytes: progress.completed_bytes,
        paused,
        idle: idle && !paused && progress.pending > 0,
        dismissed: on(db.setting(SETTING_DOWNLOAD_DISMISSED).await.ok().flatten()),
    }
}

/// Desktop's `resetBackupMediaDownloadStats`: the counts and the banner's
/// state start again.
pub(crate) async fn reset_download(db: &AppDb) {
    let _ = db.reset_backup_media_progress().await;
    for key in [SETTING_DOWNLOAD_PAUSED, SETTING_DOWNLOAD_DISMISSED] {
        let _ = db.remove_setting(key).await;
    }
}

/// Keeps the subscriber id. A new one fetches the subscription again, as
/// Desktop's `saveBackupsSubscriberData` does; returns whether it changed.
pub(crate) async fn save_subscriber_id(db: &AppDb, id: Option<&[u8]>) -> bool {
    let value = id.filter(|id| !id.is_empty()).map(hex::encode).unwrap_or_default();
    let old = db.setting(SETTING_SUBSCRIBER_ID).await.ok().flatten().unwrap_or_default();
    if old == value {
        return false;
    }
    if let Err(error) = db.set_setting(SETTING_SUBSCRIBER_ID, &value).await {
        tracing::warn!(%error, "failed to save the backup subscriber id");
    }
    true
}

/// Forgets what was fetched (the tier changed: Desktop's `resetCachedData`).
pub(crate) async fn forget_fetched(db: &AppDb) {
    for key in [SETTING_SUBSCRIPTION, SETTING_CLOUD_BACKUP] {
        let _ = db.remove_setting(key).await;
    }
}

// ----------------------------------------------------------------- fetching

/// Fetches for Settings > Backups, at most every 30 seconds each.
#[derive(Default)]
pub(crate) struct BackupStatusFetcher {
    subscription_at: Cell<Option<Instant>>,
    cloud_at: Cell<Option<Instant>>,
    /// Message backup credentials, by day, for the backup key they're for.
    credentials: tokio::sync::Mutex<Option<([u8; 32], Vec<(u64, zkgroup::backups::BackupAuthCredential)>)>>,
}

impl BackupStatusFetcher {
    /// Forgets the throttle, so the next refresh fetches (the tier or
    /// subscriber changed).
    pub(crate) fn expire(&self) {
        self.subscription_at.set(None);
        self.cloud_at.set(None);
    }

    /// `Command::RefreshBackupStatus`: what's stored at once, then what
    /// the server says. `details`: the last backup too (Desktop's details
    /// page).
    pub(crate) async fn refresh(&self, manager: &crate::actor::SignalManager, db: &AppDb, details: bool, emit: impl Fn(Event)) {
        let due = |at: &Cell<Option<Instant>>| at.get().is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
        let tier = stored_status(db, false).await.tier;
        let subscription = due(&self.subscription_at);
        let cloud = details && tier.is_some() && due(&self.cloud_at);
        emit(Event::BackupStatus(stored_status(db, subscription || cloud).await));
        if !subscription && !cloud {
            return;
        }
        if subscription {
            self.subscription_at.set(Some(Instant::now()));
            // Desktop's `#fetchSubscriptionStatus`: only a paid plan asks.
            let value = match tier {
                Some(BackupTier::Paid) => fetch_subscription(manager, db).await,
                _ => Subscription::NotFound,
            };
            let fetched = Fetched { value, at: now_millis() };
            if let Ok(json) = serde_json::to_string(&fetched) {
                let _ = db.set_setting(SETTING_SUBSCRIPTION, &json).await;
            }
        }
        if cloud {
            self.cloud_at.set(Some(Instant::now()));
            match self.fetch_cloud_backup(manager, db).await {
                Ok(value) => {
                    if let Ok(json) = serde_json::to_string(&Fetched { value, at: now_millis() }) {
                        let _ = db.set_setting(SETTING_CLOUD_BACKUP, &json).await;
                    }
                }
                // Desktop keeps what it had.
                Err(error) => tracing::warn!(error = %format!("{error:#}"), "couldn't read the last backup's details"),
            }
        }
        emit(Event::BackupStatus(stored_status(db, false).await));
    }

    /// Desktop's `getBackupProtoInfo`; `None` when there's no backup.
    async fn fetch_cloud_backup(&self, manager: &crate::actor::SignalManager, db: &AppDb) -> anyhow::Result<Option<CloudBackup>> {
        let pool = manager.store().fetch_account_entropy_pool().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let Some(pool) = pool else { bail!("no account entropy pool from the phone yet") };
        let key = BackupKey::derive_from_account_entropy_pool(&pool);
        let service = crate::storage::push_service(manager)?;
        let data = manager.registration_data();
        let params = backup_media::server_params(data.signal_servers)?;
        let aci = Aci::from(uuid::Uuid::from(data.service_ids.aci()));
        let signing_key = key.derive_ec_key(&aci);

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let today = now - now % (24 * 60 * 60);
        let credential = {
            let mut cached = self.credentials.lock().await;
            let fresh = cached.as_ref().is_some_and(|(k, list)| *k == key.0 && list.iter().any(|(day, _)| *day == today));
            if !fresh {
                match backup_media::fetch_credentials(&service, CredentialKind::Messages, &key, aci, &params, now).await? {
                    Some(list) => *cached = Some((key.0, list)),
                    None => return Ok(None),
                }
            }
            cached.as_ref().and_then(|(_, list)| list.iter().find(|(day, _)| *day == today).map(|(_, c)| c.clone())).context("no backup credential for today")?
        };
        let headers = || -> anyhow::Result<[(&'static str, String); 2]> {
            let (presentation, signature) = backup_media::present(&credential, &params, &signing_key, &mut rand::rng())?;
            Ok(backup_media::auth_headers(&presentation, &signature))
        };
        let request = |method: reqwest::Method, path: String| -> anyhow::Result<reqwest::RequestBuilder> {
            let mut request = service.request(method, Endpoint::service(path), HttpAuthOverride::Unidentified)?;
            for (name, value) in headers()? {
                request = request.header(name, value);
            }
            Ok(request)
        };

        // Desktop's `getForToday` uploads the signing key once.
        let body = backup_media::set_public_key_body(&signing_key)?;
        let public = body["backupIdPublicKey"].as_str().unwrap_or_default().to_owned();
        if db.setting(SETTING_MESSAGES_KEY_SET).await.ok().flatten().as_deref() != Some(public.as_str()) {
            request(reqwest::Method::PUT, "/v1/archives/keys".into())?
                .json(&body)
                .send()
                .await?
                .error_for_status()
                .context("uploading the backup signing key")?;
            db.set_setting(SETTING_MESSAGES_KEY_SET, &public).await?;
        }

        let response = request(reqwest::Method::GET, "/v1/archives".into())?.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.error_for_status().context("reading the backup's details")?.bytes().await?;
        let (cdn, backup_dir, backup_name) = parse_message_backup_info(&body)?;
        let body = request(reqwest::Method::GET, backup_media::read_credentials_path(cdn))?
            .send()
            .await?
            .error_for_status()
            .context("getting backup CDN credentials")?
            .bytes()
            .await?;
        let cdn_headers = backup_media::parse_read_credentials(&body)?;
        let mut head = service.request(reqwest::Method::HEAD, Endpoint::cdn(cdn, backup_file_path(&backup_dir, &backup_name)?), HttpAuthOverride::Unidentified)?;
        for (name, value) in &cdn_headers {
            head = head.header(name, value);
        }
        let response = head.send().await?;
        match response.status().as_u16() {
            200 => {}
            404 => return Ok(None),
            status => bail!("the backup CDN answered {status}"),
        }
        let header = |name: reqwest::header::HeaderName| response.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        cloud_backup_of(&header(reqwest::header::LAST_MODIFIED), &header(reqwest::header::CONTENT_LENGTH)).map(Some)
    }
}

/// Desktop's `getSubscriptionInfo`: any failure reads as not found.
async fn fetch_subscription(manager: &crate::actor::SignalManager, db: &AppDb) -> Subscription {
    let id = db.setting(SETTING_SUBSCRIBER_ID).await.ok().flatten().and_then(|h| hex::decode(h).ok()).filter(|id| !id.is_empty());
    let Some(id) = id else {
        tracing::info!("no backup subscriber id");
        return Subscription::NotFound;
    };
    let result = async {
        let service = crate::storage::push_service(manager)?;
        let body = service
            .request(reqwest::Method::GET, Endpoint::service(subscription_path(&id)), HttpAuthOverride::Unidentified)?
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        parse_subscription(&body)
    }
    .await;
    result.unwrap_or_else(|error: anyhow::Error| {
        tracing::warn!(error = %format!("{error:#}"), "couldn't fetch the backup subscription");
        Subscription::NotFound
    })
}

// --------------------------------------------------------------------- demo

/// The demo's paid plan: $1.99 a month, renewing in three weeks, backed up
/// last night; and a media download a little under half done.
pub(crate) async fn seed_demo(db: &AppDb, now: i64) {
    let day = 24 * 60 * 60 * 1000;
    let _ = db.set_setting(backup_media::SETTING_BACKUP_TIER, &u64::from(BackupLevel::Paid).to_string()).await;
    let subscription = Fetched {
        value: Subscription::Active { cost: Some(Cost { amount: 1.99, currency: "USD".into() }), renews_at: Some(now + 21 * day) },
        at: now,
    };
    let cloud = Fetched { value: Some(CloudBackup { created_at: now - day + 3 * 60 * 60 * 1000, size: 1_288_490_189 }), at: now };
    for (key, json) in [(SETTING_SUBSCRIPTION, serde_json::to_string(&subscription)), (SETTING_CLOUD_BACKUP, serde_json::to_string(&cloud))] {
        if let Ok(json) = json {
            let _ = db.set_setting(key, &json).await;
        }
    }
    // Made-up message ids: nothing downloads in the demo.
    let queued: Vec<(i64, usize, u64)> = (0..40).map(|i| (-1 - i, 0, 31_457_280)).collect();
    let _ = db.seed_backup_media_progress(1_027_604_480, &queued).await;
    reset_download_flags(db).await;
}

async fn reset_download_flags(db: &AppDb) {
    for key in [SETTING_DOWNLOAD_PAUSED, SETTING_DOWNLOAD_DISMISSED] {
        let _ = db.remove_setting(key).await;
    }
}

/// The demo's answers to the backup commands; false for any other command.
pub(crate) async fn demo_handle(db: &AppDb, command: &Command, emit: &impl Fn(Event)) -> bool {
    match command {
        Command::RefreshBackupStatus { .. } => emit(Event::BackupStatus(stored_status(db, false).await)),
        Command::LoadBackupMediaDownload => emit(Event::BackupMediaDownload(download_status(db, false).await)),
        Command::BackupMediaDownload(action) => {
            apply_action(db, *action).await;
            emit(Event::BackupMediaDownload(download_status(db, false).await));
        }
        _ => return false,
    }
    true
}

/// What a `BackupMediaDownloadAction` changes in the store (Desktop's
/// `backupMediaDownload.preload.ts`). The actor starts or stops its
/// downloads to match.
pub(crate) async fn apply_action(db: &AppDb, action: BackupMediaDownloadAction) {
    let result = match action {
        BackupMediaDownloadAction::Pause => db.set_setting(SETTING_DOWNLOAD_PAUSED, "true").await,
        BackupMediaDownloadAction::Resume => db.remove_setting(SETTING_DOWNLOAD_PAUSED).await,
        BackupMediaDownloadAction::Dismiss => db.set_setting(SETTING_DOWNLOAD_DISMISSED, "true").await,
        BackupMediaDownloadAction::Cancel => {
            // Dismissed first, as Desktop's `cancelBackupMediaDownload`,
            // then the jobs and counts go and the flags reset.
            let result = db.cancel_backup_media().await;
            reset_download_flags(db).await;
            result
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, ?action, "failed to change the backup media download");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_read_as_desktop_reads_them() {
        let active = br#"{"subscription":{"level":201,"billingCycleAnchor":1790000000,"endOfCurrentPeriod":1795000000,"active":true,"cancelAtPeriodEnd":false,"currency":"USD","amount":199}}"#;
        assert_eq!(
            parse_subscription(active).unwrap(),
            Subscription::Active { cost: Some(Cost { amount: 1.99, currency: "USD".into() }), renews_at: Some(1_795_000_000_000) }
        );
        // Yen have no hundredths.
        let canceled = br#"{"subscription":{"level":201,"endOfCurrentPeriod":1795000000,"active":true,"cancelAtPeriodEnd":true,"currency":"JPY","amount":300}}"#;
        assert_eq!(
            parse_subscription(canceled).unwrap(),
            Subscription::PendingCancellation { cost: Some(Cost { amount: 300., currency: "JPY".into() }), expires_at: Some(1_795_000_000_000) }
        );
        let expired = br#"{"subscription":{"level":201,"active":false}}"#;
        assert_eq!(parse_subscription(expired).unwrap(), Subscription::Expired);
        assert_eq!(parse_subscription(br#"{"subscription":null}"#).unwrap(), Subscription::NotFound);
        assert_eq!(parse_subscription(b"{}").unwrap(), Subscription::NotFound);
        // No amount: no cost line.
        let free = br#"{"subscription":{"level":201,"active":true}}"#;
        assert_eq!(parse_subscription(free).unwrap(), Subscription::Active { cost: None, renews_at: None });
        assert_eq!(subscription_path(&[0xfb, 0xff, 0x01]), "/v1/subscription/-_8B");
    }

    #[test]
    fn the_last_backup_is_read_from_its_file() {
        let info = br#"{"cdn":3,"backupDir":"abc","backupName":"def_-","usedSpace":null}"#;
        assert_eq!(parse_message_backup_info(info).unwrap(), (3, "abc".into(), "def_-".into()));
        assert_eq!(backup_file_path("abc", "def").unwrap(), "backups/abc/def");
        assert!(backup_file_path("../x", "def").is_err());
        let backup = cloud_backup_of("Wed, 21 Oct 2026 07:28:00 GMT", "1048576").unwrap();
        assert_eq!(backup, CloudBackup { created_at: 1_792_567_680_000, size: 1_048_576 });
        assert!(cloud_backup_of("yesterday", "1").is_err());
    }

    #[test]
    fn tiers_and_media_days() {
        assert_eq!(BackupTier::from_setting(Some("201")), Some(BackupTier::Paid));
        assert_eq!(BackupTier::from_setting(Some("200")), Some(BackupTier::Free));
        assert_eq!(BackupTier::from_setting(Some("0")), None);
        assert_eq!(BackupTier::from_setting(None), None);
        use crate::RemoteConfig;
        assert_eq!(free_media_days(&RemoteConfig::from_pairs([])), 30);
        assert_eq!(free_media_days(&RemoteConfig::from_pairs([("global.messageQueueTimeInSeconds", "3888000")])), 45);
        // Never under a month.
        assert_eq!(free_media_days(&RemoteConfig::from_pairs([("global.messageQueueTimeInSeconds", "60")])), 30);
    }

    #[test]
    fn the_spinner_shows_only_over_stale_data() {
        let status = BackupStatus {
            tier: Some(BackupTier::Paid),
            free_media_days: 30,
            subscription: None,
            subscription_fetched_at: Some(1_000_000),
            fetching: true,
            last_backup: None,
        };
        assert!(!status.show_spinner(1_000_000 + 60_000));
        assert!(status.show_spinner(1_000_000 + 6 * 60_000));
        assert!(!BackupStatus { fetching: false, ..status }.show_spinner(i64::MAX));
    }

    #[test]
    fn the_banner_shows_with_media_backups_and_something_to_count() {
        let download = BackupMediaDownload { media_backups: true, total_bytes: 10, completed_bytes: 4, ..Default::default() };
        assert!(download.show_banner() && !download.complete());
        assert!(!BackupMediaDownload { dismissed: true, ..download }.show_banner());
        assert!(!BackupMediaDownload { media_backups: false, ..download }.show_banner());
        assert!(!BackupMediaDownload { total_bytes: 0, completed_bytes: 0, ..download }.show_banner());
        assert!(BackupMediaDownload { completed_bytes: 10, ..download }.complete());
    }
}
