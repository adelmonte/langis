//! Desktop's media auto-download (Data usage, `auto-download-attachment`):
//! which kinds of attachment download as they arrive. Like Desktop
//! (`ts/util/queueAttachmentDownloads.preload.ts`), a video (a GIF too) is
//! a video, an image a photo, a voice message or a sticker within 100 KiB
//! always downloads (a bigger one follows the audio or photos choice,
//! Desktop commit a26894b08), other audio is audio, the rest documents; and
//! nothing over the remote config's limit downloads on its own. A sticker
//! over 1 MiB never downloads (`MAX_STICKER_CIPHERTEXT_BYTE_LENGTH`,
//! Desktop commit 5476a2607). Sizes are compared padded and encrypted, as
//! Desktop's `isAttachmentTooLarge` does.

use langis_store::AttachmentMeta;

/// Settings key: the choices, as JSON.
pub const SETTING_AUTO_DOWNLOAD: &str = "auto_download";

/// Desktop's `MAX_VOICE_MESSAGE_AUTO_DOWNLOAD_SIZE`.
const MAX_VOICE_BYTES: u64 = 100 * 1024;
/// Desktop's `MAX_STICKER_AUTO_DOWNLOAD_SIZE`.
const MAX_STICKER_AUTO_BYTES: u64 = 100 * 1024;
/// The biggest sticker that downloads at all (Desktop's
/// `MAX_STICKER_CIPHERTEXT_BYTE_LENGTH` in `ts/types/AttachmentSize.std.ts`),
/// a sticker message's image or one from a pack.
pub const MAX_STICKER_BYTES: u64 = 1024 * 1024;

/// Whether a sticker message's image is over [`MAX_STICKER_BYTES`]
/// (Desktop's `runDownloadAttachmentJobInner` refuses it).
pub fn sticker_too_large(attachment: &AttachmentMeta) -> bool {
    attachment.sticker && crate::remote_config::too_large(attachment.size, MAX_STICKER_BYTES)
}

/// Desktop's `AutoDownloadAttachmentType`; all on by default
/// (`DEFAULT_AUTO_DOWNLOAD_ATTACHMENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoDownload {
    #[serde(default = "on")]
    pub photos: bool,
    #[serde(default = "on")]
    pub videos: bool,
    #[serde(default = "on")]
    pub audio: bool,
    #[serde(default = "on")]
    pub documents: bool,
}

fn on() -> bool {
    true
}

impl Default for AutoDownload {
    fn default() -> Self {
        Self { photos: true, videos: true, audio: true, documents: true }
    }
}

impl AutoDownload {
    /// From the stored JSON (Desktop's defaults when there's none).
    pub fn from_setting(value: Option<&str>) -> Self {
        value.and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default()
    }

    /// Whether `attachment` downloads as it arrives (Desktop's
    /// `queueAttachmentDownloads` and `isAutoDownloadEnabled`).
    pub fn downloads(&self, attachment: &AttachmentMeta) -> bool {
        let kind = attachment.content_type.as_str();
        if sticker_too_large(attachment) {
            return false;
        }
        let small_voice = attachment.voice && !crate::remote_config::too_large(attachment.size, MAX_VOICE_BYTES);
        let small_sticker = attachment.sticker && !crate::remote_config::too_large(attachment.size, MAX_STICKER_AUTO_BYTES);
        if small_sticker || small_voice {
            return true;
        }
        let chosen = if attachment.sticker {
            self.photos
        } else if kind.starts_with("video/") || attachment.gif {
            self.videos
        } else if kind.starts_with("image/") {
            self.photos
        } else if kind.starts_with("audio/") || attachment.voice {
            self.audio
        } else {
            self.documents
        };
        chosen && crate::remote_config().auto_downloads(attachment.size)
    }
}

thread_local! {
    /// The choices of the account whose backend thread this is (each
    /// account has its own; all its actor work runs on that thread).
    static CURRENT: std::cell::Cell<Option<AutoDownload>> = const { std::cell::Cell::new(None) };
}

/// The choices in effect for this thread's account.
pub fn current() -> AutoDownload {
    CURRENT.with(|current| current.get()).unwrap_or_default()
}

pub(crate) fn set_current(choices: AutoDownload) {
    CURRENT.with(|current| current.set(Some(choices)));
}

/// Puts the saved choices in effect.
pub(crate) async fn load(db: &langis_store::AppDb) {
    let value = db.setting(SETTING_AUTO_DOWNLOAD).await.ok().flatten();
    set_current(AutoDownload::from_setting(value.as_deref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(content_type: &str, size: u64) -> AttachmentMeta {
        serde_json::from_value(serde_json::json!({
            "content_type": content_type, "file_name": null, "size": size, "width": null, "height": null, "local_path": null
        }))
        .unwrap()
    }

    #[test]
    fn kinds_follow_desktops_choices() {
        let all = AutoDownload::default();
        for kind in ["image/jpeg", "video/mp4", "audio/aac", "application/pdf"] {
            assert!(all.downloads(&meta(kind, 1000)), "{kind}");
        }
        let none = AutoDownload { photos: false, videos: false, audio: false, documents: false };
        for kind in ["image/jpeg", "video/mp4", "audio/aac", "application/pdf"] {
            assert!(!none.downloads(&meta(kind, 1000)), "{kind}");
        }
        // A GIF is a video; a short voice message and a sticker always come.
        let gif = AttachmentMeta { gif: true, ..meta("video/mp4", 1000) };
        assert!(!AutoDownload { videos: false, ..all }.downloads(&gif));
        assert!(AutoDownload { photos: false, ..all }.downloads(&gif));
        let voice = AttachmentMeta { voice: true, ..meta("audio/aac", 50_000) };
        assert!(none.downloads(&voice));
        let long_voice = AttachmentMeta { voice: true, ..meta("audio/aac", 500_000) };
        assert!(!none.downloads(&long_voice) && all.downloads(&long_voice));
        // 100 KiB less a byte is over 100 KiB once padded and encrypted.
        let edge_voice = AttachmentMeta { voice: true, ..meta("audio/aac", 100 * 1024 - 1) };
        assert!(!none.downloads(&edge_voice));
        let sticker = AttachmentMeta { sticker: true, ..meta("image/webp", 20_000) };
        assert!(none.downloads(&sticker));
        // A bigger sticker follows photos (Desktop commit a26894b08); one
        // over 1 MiB never comes (5476a2607).
        let big_sticker = AttachmentMeta { sticker: true, ..meta("image/webp", 200_000) };
        assert!(!none.downloads(&big_sticker) && all.downloads(&big_sticker));
        assert!(!AutoDownload { photos: false, ..all }.downloads(&big_sticker));
        let huge_sticker = AttachmentMeta { sticker: true, ..meta("image/webp", 1024 * 1024) };
        assert!(sticker_too_large(&huge_sticker) && !all.downloads(&huge_sticker));
        assert!(!sticker_too_large(&big_sticker));
        // Over the remote config's limit, nothing downloads on its own.
        assert!(!all.downloads(&meta("image/jpeg", crate::remote_config().auto_download_max_bytes())));
        // Stored choices read back, a missing one on.
        let only = AutoDownload::from_setting(Some(r#"{"photos":false}"#));
        assert_eq!(only, AutoDownload { photos: false, ..all });
        assert_eq!(AutoDownload::from_setting(None), all);
    }
}
