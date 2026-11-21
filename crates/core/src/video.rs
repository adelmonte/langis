//! Videos made smaller before sending, as Signal Desktop does from 8.37
//! (commit 60d50a3: `ts/util/transcodeAttachment.preload.ts`,
//! `ts/util/getTranscodingOptions.preload.ts`): an MP4 is re-encoded as
//! H.264 in an MP4 with its shorter side at most the target resolution and
//! its video bitrate at most the target's (never raised, commit 68662e4),
//! keeping one video and one audio track, written for streaming (Desktop's
//! `fastStart`), without the original's metadata. Desktop sends at its
//! standard quality unless the sender picks high quality for that message;
//! we have no such choice, so it's always standard.
//!
//! The encoding is done by the system's `ffmpeg` and `ffprobe` (with
//! libx264), run as programs. Without them, videos go as they are, as
//! Desktop sends a video it can't transcode.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context as _, bail};

/// One quality step of Desktop's `TranscodingConfigSchema`: for videos up
/// to `max_duration_secs` long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranscodeTarget {
    /// The shorter side, in pixels.
    pub resolution: u32,
    pub video_bitrate_mbps: f64,
    pub max_duration_secs: f64,
}

const fn target(resolution: u32, video_bitrate_mbps: f64, max_duration_secs: f64) -> TranscodeTarget {
    TranscodeTarget { resolution, video_bitrate_mbps, max_duration_secs }
}

/// Desktop's `DEFAULT_OPTIONS` (standard quality), by our number's country
/// calling code. Desktop reads `client.attachments.videoTranscodingConfiguration`
/// first, but its config fetch keeps only `desktop.`, `global.` and `cds.`
/// keys (`ts/textsecure/WebAPI.preload.ts`), as ours does, so these are
/// what applies.
const REGIONS: [(&[u16], &[TranscodeTarget]); 2] = [
    (&[], &[target(480, 1.0, 900.)]),
    (
        &[1, 61, 81, 82, 65, 31, 47, 41, 32, 385, 971, 974, 49, 33],
        &[target(720, 2.0, 600.), target(720, 1.5, 900.)],
    ),
];

/// The steps for our number (E.164, `+` and digits): its country's, else
/// the default ones (Desktop's `getRegionConfig`). Country calling codes
/// are prefix-free, so the number's start names its country.
fn region(phone: Option<&str>) -> &'static [TranscodeTarget] {
    let digits = phone.map(|p| p.trim_start_matches('+')).unwrap_or_default();
    REGIONS
        .iter()
        .find(|(codes, _)| !digits.is_empty() && codes.iter().any(|code| digits.starts_with(&code.to_string())))
        .or_else(|| REGIONS.iter().find(|(codes, _)| codes.is_empty()))
        .map_or(&[], |(_, steps)| *steps)
}

/// The step a video of `duration_secs` is encoded with: the first long
/// enough, else the last (Desktop's `getTranscodingOptions`).
pub fn target_for(phone: Option<&str>, duration_secs: f64) -> TranscodeTarget {
    let steps = region(phone);
    steps.iter().find(|s| duration_secs <= s.max_duration_secs).or(steps.last()).copied().unwrap_or(REGIONS[0].1[0])
}

/// The longest video that can be sent, in seconds (Desktop's
/// `getMaxVideoDuration`: the last step's).
pub fn max_duration_secs(phone: Option<&str>) -> f64 {
    target_for(phone, f64::INFINITY).max_duration_secs
}

/// "15 minutes", as Desktop's `VideoFileDuration` toast writes the limit
/// (`Intl.DurationFormat` of whole minutes).
pub fn duration_label(secs: f64) -> String {
    let minutes = (secs / 60.).floor() as u64;
    if minutes == 1 { "1 minute".into() } else { format!("{minutes} minutes") }
}

/// Whether a video of this type can be re-encoded: MP4, QuickTime (.mov),
/// Ogg and WebM, as Desktop's `isVideoTypeTranscodable`
/// (`ts/util/GoogleChrome.std.ts`, QuickTime since Desktop commit
/// f820b4af7; only these formats are read, commit 508d12f69).
pub fn transcodable(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(base.as_str(), "video/mp4" | "video/quicktime" | "video/ogg" | "video/webm")
}

/// Whether a video of this type is re-encoded before sending: one that's
/// [`transcodable`], when ffmpeg is installed. What comes out is an MP4
/// (`video/mp4`, as Desktop's `transcodeVideo` sets it).
pub fn transcodes(content_type: &str) -> bool {
    transcodable(content_type) && tools().is_some()
}

/// Where `ffmpeg` and `ffprobe` are, looked up once: both on `PATH`, and
/// this ffmpeg can encode H.264 with libx264.
fn tools() -> Option<&'static (PathBuf, PathBuf)> {
    static TOOLS: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            let find = |name: &str| {
                std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|path| path.is_file())
            };
            let (ffmpeg, ffprobe) = (find("ffmpeg")?, find("ffprobe")?);
            let encoders = Command::new(&ffmpeg).args(["-hide_banner", "-encoders"]).output().ok()?;
            if !String::from_utf8_lossy(&encoders.stdout).contains("libx264") {
                tracing::info!("ffmpeg has no libx264; videos are sent as they are");
                return None;
            }
            Some((ffmpeg, ffprobe))
        })
        .as_ref()
}

/// A path ffmpeg reads as a plain file, whatever its name (a `:` would
/// otherwise name a protocol, a leading `-` an option).
fn file_arg(path: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("file:");
    arg.push(path.as_os_str());
    arg
}

/// What a video holds, read before it's sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub duration_secs: f64,
    /// The video track's bitrate (bits per second).
    pub video_bps: f64,
    /// The audio track's, or 0 without one.
    pub audio_bps: f64,
    /// Its sound is AAC, copied as it is; any other goes as 128 kbps AAC.
    pub audio_copied: bool,
    /// The size it's shown at, after its rotation.
    pub display_width: u32,
    pub display_height: u32,
}

/// The bitrate sound other than AAC is re-encoded at.
const REENCODED_AUDIO_BPS: f64 = 128_000.;

/// About how big the video comes out at `target`: its tracks' bitrates
/// over its length, the video's at most the target's. Desktop encodes it
/// once to find out (`estimateTranscodedVideoSize`); this is quicker and
/// close enough to decide whether it can be sent.
/// A video whose bitrate isn't known is taken at the target's.
pub fn estimated_size(info: &VideoInfo, target: &TranscodeTarget) -> u64 {
    let wanted = target.video_bitrate_mbps * 1e6;
    let video = if info.video_bps > 0. { info.video_bps.min(wanted) } else { wanted };
    let audio = if info.audio_copied || info.audio_bps == 0. { info.audio_bps } else { REENCODED_AUDIO_BPS };
    ((video + audio) * info.duration_secs / 8.).ceil() as u64
}

/// The size the video is encoded at, before its rotation is applied: its
/// shorter displayed side at most `resolution` (Desktop: a wider video's
/// height, otherwise its width), even numbers as H.264 wants.
/// `natural`: its stored size; `rotated`: displayed a quarter turn round.
pub fn encoded_size(natural: (u32, u32), rotated: bool, resolution: u32) -> (u32, u32) {
    let (w, h) = natural;
    let (display_w, display_h) = if rotated { (h, w) } else { (w, h) };
    let short = if display_w > display_h { display_h } else { display_w };
    let scale = if short > resolution && short > 0 { f64::from(resolution) / f64::from(short) } else { 1. };
    let even = |n: u32| ((f64::from(n) * scale / 2.).round() as u32 * 2).max(2);
    (even(w), even(h))
}

/// Reads a video's length, bitrates and size with ffprobe. `None` when it
/// can't be read as a video (or there's no ffprobe).
pub fn probe(path: &Path) -> Option<VideoInfo> {
    let (_, ffprobe) = tools()?;
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(file_arg(path))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_probe(&serde_json::from_slice(&output.stdout).ok()?)
}

/// `probe`'s reading of ffprobe's JSON.
fn parse_probe(json: &serde_json::Value) -> Option<VideoInfo> {
    // ffprobe writes numbers as strings.
    let number = |value: &serde_json::Value| -> Option<f64> {
        value.as_f64().or_else(|| value.as_str()?.parse().ok()).filter(|n: &f64| n.is_finite())
    };
    let streams = json.get("streams")?.as_array()?;
    let kind = |stream: &&serde_json::Value, kind: &str| stream.get("codec_type").and_then(|t| t.as_str()) == Some(kind);
    let video = streams.iter().find(|s| kind(s, "video"))?;
    let audio = streams.iter().find(|s| kind(s, "audio"));
    let (width, height) = (video.get("width")?.as_u64()? as u32, video.get("height")?.as_u64()? as u32);
    let duration = json.pointer("/format/duration").and_then(number).or_else(|| video.get("duration").and_then(number))?;
    // A quarter turn (phones store portrait video as landscape, turned).
    let rotation = video
        .get("side_data_list")
        .and_then(|list| list.as_array())
        .and_then(|list| list.iter().find_map(|d| d.get("rotation").and_then(number)))
        .or_else(|| video.pointer("/tags/rotate").and_then(number))
        .unwrap_or(0.);
    let turned = (rotation.abs().round() as i64 / 90) % 2 == 1;
    let audio_bps = audio.and_then(|a| a.get("bit_rate")).and_then(number).unwrap_or(0.);
    let audio_bps = if audio.is_some() && audio_bps == 0. { REENCODED_AUDIO_BPS } else { audio_bps };
    let video_bps = video.get("bit_rate").and_then(number).or_else(|| {
        // Not given per stream (WebM): the whole file's, less the sound.
        Some(json.pointer("/format/bit_rate").and_then(number)? - audio_bps).filter(|b| *b > 0.)
    });
    let audio_copied = audio.and_then(|a| a.get("codec_name")).and_then(|c| c.as_str()) == Some("aac");
    let (display_width, display_height) = if turned { (height, width) } else { (width, height) };
    Some(VideoInfo { duration_secs: duration, video_bps: video_bps.unwrap_or(0.), audio_bps, audio_copied, display_width, display_height })
}

/// Encodes `input` to `output` (a new file) at `target` with ffmpeg:
/// H.264 in an MP4 for streaming, its shorter shown side at most the
/// target's, its bitrate at most the target's and never above the
/// original's, AAC sound (copied when it already is), no metadata. The
/// caller decides whether the result is worth sending.
pub fn transcode(input: &Path, output: &Path, target: &TranscodeTarget) -> anyhow::Result<()> {
    let (ffmpeg, _) = tools().context("ffmpeg isn't installed")?;
    let info = probe(input).context("not a video ffprobe can read")?;
    // ffmpeg turns the picture upright before the scale filter, so the
    // size is the shown one.
    let (width, height) = encoded_size((info.display_width, info.display_height), false, target.resolution);
    let wanted = target.video_bitrate_mbps * 1e6;
    let bitrate = if info.video_bps > 0. { info.video_bps.min(wanted) } else { wanted } as u64;
    let mut command = Command::new(ffmpeg);
    command
        .args(["-v", "error", "-nostdin", "-n", "-i"])
        .arg(file_arg(input))
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-vf", &format!("scale={width}:{height}")])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"])
        .args(["-b:v", &bitrate.to_string(), "-maxrate", &bitrate.to_string(), "-bufsize", &(bitrate * 2).to_string()]);
    if info.audio_copied {
        command.args(["-c:a", "copy"]);
    } else {
        command.args(["-c:a", "aac", "-b:a", &(REENCODED_AUDIO_BPS as u64).to_string()]);
    }
    let result = command.args(["-movflags", "+faststart", "-f", "mp4"]).arg(file_arg(output)).output().context("running ffmpeg")?;
    if !result.status.success() {
        let error = String::from_utf8_lossy(&result.stderr);
        bail!("ffmpeg failed: {}", error.lines().last().unwrap_or_default());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quicktime_is_transcodable_and_other_videos_are_not() {
        for kind in ["video/mp4", "video/quicktime", "video/webm", "video/ogg", "VIDEO/QuickTime; codecs=x"] {
            assert!(transcodable(kind), "{kind}");
        }
        for kind in ["video/x-matroska", "video/x-msvideo", "image/gif", "audio/mp4"] {
            assert!(!transcodable(kind), "{kind}");
        }
    }

    #[test]
    fn targets_follow_desktops_defaults_by_country() {
        // Elsewhere: 480p at 1 Mbps, up to 15 minutes.
        assert_eq!(target_for(Some("+447700900123"), 60.), target(480, 1.0, 900.));
        assert_eq!(target_for(None, 60.), target(480, 1.0, 900.));
        // The US and others: 720p, 2 Mbps up to 10 minutes, then 1.5.
        assert_eq!(target_for(Some("+15555550123"), 600.), target(720, 2.0, 600.));
        assert_eq!(target_for(Some("+15555550123"), 601.), target(720, 1.5, 900.));
        assert_eq!(target_for(Some("+38591234567"), 60.).resolution, 720);
        // Longer than every step: the last.
        assert_eq!(target_for(Some("+4915123456789"), 5_000.), target(720, 1.5, 900.));
        assert_eq!(max_duration_secs(Some("+15555550123")), 900.);
        assert_eq!(duration_label(900.), "15 minutes");
    }

    #[test]
    fn the_shorter_side_is_scaled_down_never_up() {
        // Landscape 1080p to 480p: height 480.
        assert_eq!(encoded_size((1920, 1080), false, 480), (854, 480));
        // Portrait, stored landscape and turned: the shown width is 480.
        assert_eq!(encoded_size((1920, 1080), true, 480), (854, 480));
        assert_eq!(encoded_size((1080, 1920), false, 720), (720, 1280));
        // Already small enough.
        assert_eq!(encoded_size((640, 360), false, 720), (640, 360));
    }

    /// A real re-encode, of a clip ffmpeg makes (skipped without it).
    #[test]
    fn a_1080p_clip_comes_out_480p_with_its_sound() {
        let dir = tempfile::tempdir().unwrap();
        let (input, output) = (dir.path().join("in.mp4"), dir.path().join("out.mp4"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=1920x1080:rate=30:duration=3"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-c:v", "libx264", "-b:v", "8M"])
            // Constant bitrate: a test pattern would otherwise come out tiny.
            .args(["-minrate", "8M", "-maxrate", "8M", "-bufsize", "2M", "-x264-params", "nal-hrd=cbr"])
            .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&input)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let before = probe(&input).expect("the clip reads");
        assert_eq!((before.display_width, before.display_height), (1920, 1080));
        transcode(&input, &output, &target(480, 1.0, 900.)).unwrap();
        let after = probe(&output).expect("the result reads");
        assert_eq!((after.display_width, after.display_height), (854, 480));
        assert!((after.duration_secs - before.duration_secs).abs() < 0.2, "{after:?}");

        // Sound an MP4 can't take as it is (ALAC here) is re-encoded as AAC.
        let (alac, alac_out) = (dir.path().join("alac.mp4"), dir.path().join("alac-out.mp4"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&input)
            .args(["-c:v", "copy", "-c:a", "alac"])
            .arg(&alac)
            .status();
        assert!(made.is_ok_and(|s| s.success()));
        transcode(&alac, &alac_out, &target(480, 1.0, 900.)).unwrap();
        assert!(probe(&alac_out).unwrap().audio_bps > 0., "the sound is kept");
        assert!(after.audio_bps > 0., "the sound is kept");
        let (size_in, size_out) = (std::fs::metadata(&input).unwrap().len(), std::fs::metadata(&output).unwrap().len());
        assert!(size_out < size_in / 2, "{size_in} -> {size_out}");

        // Stored landscape and shown upright, as phones record: still shown
        // upright, its shown width 480.
        let (turned, turned_out) = (dir.path().join("turned.mp4"), dir.path().join("turned-out.mp4"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-display_rotation", "90", "-i"])
            .arg(&input)
            .args(["-c", "copy"])
            .arg(&turned)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("this ffmpeg can't set a rotation; skipping that part");
            return;
        }
        let before = probe(&turned).unwrap();
        assert_eq!((before.display_width, before.display_height), (1080, 1920));
        transcode(&turned, &turned_out, &target(480, 1.0, 900.)).unwrap();
        let after = probe(&turned_out).unwrap();
        assert_eq!((after.display_width, after.display_height), (480, 854));
    }

    #[test]
    fn probes_read_size_turn_and_bitrates() {
        let json = serde_json::json!({
            "streams": [
                {"codec_type": "video", "width": 1920, "height": 1080, "bit_rate": "8000000",
                 "side_data_list": [{"rotation": -90}]},
                {"codec_type": "audio", "codec_name": "opus"}
            ],
            "format": {"duration": "12.5", "bit_rate": "8200000"}
        });
        let info = parse_probe(&json).unwrap();
        assert_eq!((info.display_width, info.display_height), (1080, 1920));
        assert_eq!(info.duration_secs, 12.5);
        assert_eq!(info.video_bps, 8e6);
        // Sound with no bitrate given counts as what it'll be re-encoded at.
        assert_eq!(info.audio_bps, REENCODED_AUDIO_BPS);
        assert!(!info.audio_copied);
        // No video at all: not a video.
        assert!(parse_probe(&serde_json::json!({"streams": [{"codec_type": "audio"}], "format": {}})).is_none());
        // WebM gives no per-stream bitrate: the file's, less the sound.
        let webm = serde_json::json!({
            "streams": [{"codec_type": "video", "width": 640, "height": 360}],
            "format": {"duration": "1", "bit_rate": "500000"}
        });
        assert_eq!(parse_probe(&webm).unwrap().video_bps, 5e5);
    }

    #[test]
    fn the_estimate_caps_the_video_bitrate() {
        let info = VideoInfo {
            duration_secs: 80.,
            video_bps: 8e6,
            audio_bps: 128e3,
            audio_copied: true,
            display_width: 1920,
            display_height: 1080,
        };
        // 1 Mbps of video and 128 kbps of audio for 80 s.
        assert_eq!(estimated_size(&info, &target(480, 1.0, 900.)), 11_280_000);
        let low = VideoInfo { video_bps: 5e5, ..info };
        assert_eq!(estimated_size(&low, &target(480, 1.0, 900.)), 6_280_000);
        // Unknown: the target's.
        let unknown = VideoInfo { video_bps: 0., ..info };
        assert_eq!(estimated_size(&unknown, &target(480, 1.0, 900.)), 11_280_000);
        // PCM sound at 1.5 Mbps goes as 128 kbps AAC.
        let pcm = VideoInfo { audio_bps: 1_536e3, audio_copied: false, ..info };
        assert_eq!(estimated_size(&pcm, &target(480, 1.0, 900.)), 11_280_000);
    }
}
