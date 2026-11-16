//! Screen sharing (Desktop's "Present"): a display or window sent in place
//! of the camera.
//!
//! Not implemented yet. On Linux it belongs on the XDG desktop portal's
//! ScreenCast interface (PipeWire), which works on Wayland and X11 and lets
//! the desktop ask the user what to share. Until then [`sources`] lists
//! nothing and [`Share::start`] fails, so a frontend can hide the button.

use ringrtc::webrtc::media::VideoSource;

use crate::FrameSlot;

/// Something to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSource {
    /// `display:<id>` or `window:<id>`.
    pub id: String,
    pub name: String,
    pub is_display: bool,
}

/// What can be shared: nothing yet.
pub fn sources() -> anyhow::Result<Vec<ShareSource>> {
    Ok(Vec::new())
}

/// A running share. None can start yet.
pub struct Share;

impl Share {
    pub fn start(_source: &str, _video: VideoSource, _local: FrameSlot) -> anyhow::Result<Self> {
        anyhow::bail!("screen sharing isn't available yet")
    }
}
