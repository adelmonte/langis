//! Screen sharing (Desktop's "Present"): a screen or window sent in place
//! of the camera.
//!
//! On Linux this goes through the XDG desktop portal's ScreenCast
//! interface, which works on Wayland and X11 alike: the desktop shows its
//! own picker (so there's one source to list, [`PORTAL_SOURCE`]), then
//! hands over a PipeWire stream of what the user picked. Each frame is
//! turned into RGBA, scaled to the chosen [`ShareQuality`], and pushed to
//! RingRTC's video source and the self-view.
//!
//! A share runs on its own thread, from the portal's dialog to the end of
//! the stream. It tells the caller when it really started (the user picked
//! something) and when it ended on its own (the user cancelled, stopped it
//! from the desktop's indicator, or the window closed).

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa;
use ringrtc::webrtc::media::{VideoFrame, VideoPixelFormat, VideoSource};

use crate::FrameSlot;

/// Something to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSource {
    /// `portal` on Linux (see [`PORTAL_SOURCE`]).
    pub id: String,
    pub name: String,
    pub is_display: bool,
}

/// The one source on Linux: the desktop's own picker chooses the screen or
/// window once sharing starts.
pub const PORTAL_SOURCE: &str = "portal";

/// How a share is sent: at most this tall (keeping its shape) and this
/// many frames a second. Larger and faster looks better and costs more
/// bandwidth and CPU; WebRTC lowers the bitrate itself on a poor network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareQuality {
    /// Height in pixels; 0 for the screen's own size (up to 4096×2160).
    pub max_height: u32,
    /// Frames per second, 1 to 60.
    pub fps: u32,
}

impl Default for ShareQuality {
    /// Desktop's: full HD at 15 frames a second.
    fn default() -> Self {
        Self::STANDARD
    }
}

impl ShareQuality {
    /// Sharp text, little motion: the screen's own size, 5 fps.
    pub const TEXT: Self = Self { max_height: 0, fps: 5 };
    /// Desktop's: 1080p, 15 fps.
    pub const STANDARD: Self = Self { max_height: 1080, fps: 15 };
    /// For video or games on a modest link: 720p, 30 fps.
    pub const SMOOTH: Self = Self { max_height: 720, fps: 30 };
    /// 1440p, 30 fps.
    pub const HIGH: Self = Self { max_height: 1440, fps: 30 };
    /// Everything: the screen's own size, 60 fps.
    pub const MAX: Self = Self { max_height: 0, fps: 60 };

    /// Named presets, for a settings menu (any other value works too).
    pub const PRESETS: [(&'static str, Self); 5] = [
        ("Text (full resolution, 5 fps)", Self::TEXT),
        ("Standard (1080p, 15 fps)", Self::STANDARD),
        ("Smooth (720p, 30 fps)", Self::SMOOTH),
        ("High (1440p, 30 fps)", Self::HIGH),
        ("Maximum (full resolution, 60 fps)", Self::MAX),
    ];

    /// Within what's allowed: 1 to 60 fps, and a height of 0 or 144 to 2160.
    pub fn clamped(self) -> Self {
        Self {
            max_height: if self.max_height == 0 { 0 } else { self.max_height.clamp(144, MAX_SIZE.1) },
            fps: self.fps.clamp(1, 60),
        }
    }

    /// As kept in settings: `1080@15`.
    pub fn to_setting(self) -> String {
        format!("{}@{}", self.max_height, self.fps)
    }

    pub fn from_setting(text: &str) -> Option<Self> {
        let (height, fps) = text.trim().split_once('@')?;
        Some(Self { max_height: height.parse().ok()?, fps: fps.parse().ok()? }.clamped())
    }
}

/// The largest frame sent, whatever the quality: what receivers take
/// (`MAX_FRAME` in lib.rs takes 4096 wide).
const MAX_SIZE: (u32, u32) = (4096, 2160);

/// With no new frame (the screen didn't change), the last one goes again
/// this often, so someone joining (or a lost keyframe) still gets a picture.
const REPEAT: Duration = Duration::from_secs(1);

/// What a share tells its starter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareEvent {
    /// The user picked something and frames are on the way.
    Started,
    /// It ended without being stopped: cancelled in the picker or from
    /// the desktop (`None`), or failed.
    Ended(Option<String>),
}

/// Whether the desktop can share: its portal answers with ScreenCast.
pub fn sources() -> anyhow::Result<Vec<ShareSource>> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let available = runtime.block_on(async {
        let proxy = Screencast::new().await?;
        proxy.available_source_types().await
    });
    match available {
        Ok(types) if types.intersects(SourceType::MONITOR | SourceType::WINDOW) => Ok(vec![ShareSource {
            id: PORTAL_SOURCE.into(),
            name: "Choose a screen or window".into(),
            is_display: true,
        }]),
        Ok(_) => Ok(Vec::new()),
        Err(error) => {
            tracing::info!(%error, "no ScreenCast portal; screen sharing unavailable");
            Ok(Vec::new())
        }
    }
}

/// What a running share and its thread both see.
struct Shared {
    quality: Mutex<ShareQuality>,
    stop: Mutex<Stop>,
}

#[derive(Default)]
struct Stop {
    stopped: bool,
    /// Ends the portal's wait (the dialog may be open).
    portal: Option<tokio::sync::oneshot::Sender<()>>,
    /// Ends the PipeWire loop.
    stream: Option<pw::channel::Sender<()>>,
}

/// A running share; dropping it stops it.
pub struct Share {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Share {
    /// Opens the desktop's picker and, once the user picks, sends what
    /// they picked. Returns at once; `events` hears how it goes (never
    /// after the share is dropped).
    pub fn start(
        source: &str,
        video: VideoSource,
        preview: FrameSlot,
        quality: ShareQuality,
        events: impl Fn(ShareEvent) + Send + 'static,
    ) -> anyhow::Result<Self> {
        if source != PORTAL_SOURCE {
            bail!("unknown source {source}");
        }
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let shared = Arc::new(Shared {
            quality: Mutex::new(quality.clamped()),
            stop: Mutex::new(Stop { portal: Some(cancel), ..Default::default() }),
        });
        let thread_shared = shared.clone();
        let sink: Sink = Box::new(move |width, height, rgba: &[u8]| {
            video.push_frame(VideoFrame::copy_from_slice(width, height, VideoPixelFormat::Rgba, rgba));
            preview.put_rgba(width, height, rgba, false);
        });
        let thread = std::thread::Builder::new()
            .name("screen-share".into())
            .spawn(move || {
                let result = run(&thread_shared, cancelled, sink, &events);
                if thread_shared.stop.lock().unwrap().stopped {
                    return;
                }
                match result {
                    Ok(()) => events(ShareEvent::Ended(None)),
                    Err(error) => {
                        let error = format!("{error:#}");
                        tracing::warn!(%error, "screen share stopped");
                        events(ShareEvent::Ended(Some(error)));
                    }
                }
            })
            .context("starting the screen share thread")?;
        Ok(Self { shared, thread: Some(thread) })
    }

    /// Changes the quality while sharing; the next frame follows it.
    pub fn set_quality(&self, quality: ShareQuality) {
        *self.shared.quality.lock().unwrap() = quality.clamped();
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        {
            let mut stop = self.shared.stop.lock().unwrap();
            stop.stopped = true;
            if let Some(portal) = stop.portal.take() {
                let _ = portal.send(());
            }
            if let Some(stream) = stop.stream.take() {
                let _ = stream.send(());
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The share, start to end. `Ok` when it ended without an error.
fn run(
    shared: &Arc<Shared>,
    cancelled: tokio::sync::oneshot::Receiver<()>,
    sink: Sink,
    events: &dyn Fn(ShareEvent),
) -> anyhow::Result<()> {
    // Kept until the stream ends: zbus answers the portal on it, and
    // the session lives as long as the connection does.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
    let picked = runtime.block_on(async {
        tokio::select! {
            picked = open_portal() => picked.map(Some),
            _ = cancelled => Ok(None),
        }
    })?;
    let Some((proxy, session, node, fd)) = picked else { return Ok(()) };
    if shared.stop.lock().unwrap().stopped {
        return Ok(());
    }
    let result = stream(shared, node, fd, sink, events);
    let _ = runtime.block_on(async {
        let _ = session.close().await;
        drop(proxy);
    });
    result
}

type Picked = (Screencast, ashpd::desktop::Session<Screencast>, u32, OwnedFd);

/// The portal's dialog: `Ok(None)`-like cancels come back as an error
/// `run` turns into a quiet end.
async fn open_portal() -> anyhow::Result<Picked> {
    let proxy = Screencast::new().await.context("the desktop has no ScreenCast portal")?;
    let types = proxy.available_source_types().await.unwrap_or(SourceType::MONITOR);
    let cursors = proxy.available_cursor_modes().await.unwrap_or(CursorMode::HIDDEN);
    // The pointer drawn into the picture, as Desktop shares it.
    let cursor = if cursors.contains(CursorMode::EMBEDDED) { CursorMode::EMBEDDED } else { CursorMode::HIDDEN };
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(types & (SourceType::MONITOR | SourceType::WINDOW))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = match proxy.start(&session, None, Default::default()).await?.response() {
        Ok(response) => response,
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => return Err(Cancelled.into()),
        Err(error) => return Err(error.into()),
    };
    let node = response.streams().first().ok_or_else(|| anyhow!("nothing was picked"))?.pipe_wire_node_id();
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
    Ok((proxy, session, node, fd))
}

/// The user closed the picker without sharing.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Pixel layouts we take from PipeWire (what compositors give for screens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
}

impl Layout {
    fn from_spa(format: spa::param::video::VideoFormat) -> Option<Self> {
        use spa::param::video::VideoFormat as F;
        match format {
            F::BGRx => Some(Self::Bgrx),
            F::BGRA => Some(Self::Bgra),
            F::RGBx => Some(Self::Rgbx),
            F::RGBA => Some(Self::Rgba),
            _ => None,
        }
    }
}

/// Where frames go: each one's width, height and RGBA bytes.
type Sink = Box<dyn FnMut(u32, u32, &[u8]) + Send>;

/// The frame we're building, shared by the stream's callbacks and the
/// repeat timer (all on the PipeWire thread).
struct Pipeline {
    shared: Arc<Shared>,
    sink: Sink,
    layout: Option<Layout>,
    size: (u32, u32),
    last_push: Option<Instant>,
    /// The last frame sent (RGBA, its size), to repeat.
    last: Option<(u32, u32, Vec<u8>)>,
    resizer: fast_image_resize::Resizer,
}

impl Pipeline {
    fn quality(&self) -> ShareQuality {
        *self.shared.quality.lock().unwrap()
    }

    /// One frame from PipeWire: kept if it's time for one.
    fn frame(&mut self, bytes: &[u8], stride: usize) {
        let Some(layout) = self.layout else { return };
        let quality = self.quality();
        let now = Instant::now();
        if let Some(last) = self.last_push
            && now.duration_since(last) < Duration::from_secs(1) / quality.fps.max(1)
        {
            return;
        }
        let (width, height) = self.size;
        let Some(rgba) = to_rgba(layout, width as usize, height as usize, stride, bytes) else { return };
        let (to_width, to_height) = fit(width, height, quality.max_height);
        let (rgba, width, height) = if (to_width, to_height) == (width, height) {
            (rgba, width, height)
        } else {
            match self.scale(rgba, (width, height), (to_width, to_height)) {
                Some(scaled) => (scaled, to_width, to_height),
                None => return,
            }
        };
        self.push(width, height, rgba, now);
    }

    fn scale(&mut self, rgba: Vec<u8>, from: (u32, u32), to: (u32, u32)) -> Option<Vec<u8>> {
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, images::Image};
        let source = Image::from_vec_u8(from.0, from.1, rgba, PixelType::U8x4).ok()?;
        let mut target = Image::new(to.0, to.1, PixelType::U8x4);
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
        self.resizer.resize(&source, &mut target, &options).ok()?;
        Some(target.into_vec())
    }

    fn push(&mut self, width: u32, height: u32, rgba: Vec<u8>, now: Instant) {
        (self.sink)(width, height, &rgba);
        self.last_push = Some(now);
        self.last = Some((width, height, rgba));
    }

    /// The last frame again, if none came for a while.
    fn repeat(&mut self) {
        let now = Instant::now();
        if self.last_push.is_some_and(|last| now.duration_since(last) < REPEAT) {
            return;
        }
        if let Some((width, height, rgba)) = self.last.take() {
            self.push(width, height, rgba, now);
        }
    }
}

/// Reads the PipeWire stream until it ends or the share is stopped.
fn stream(
    shared: &Arc<Shared>,
    node: u32,
    fd: OwnedFd,
    sink: Sink,
    events: &dyn Fn(ShareEvent),
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("starting PipeWire")?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect_fd(fd, None).context("connecting to the shared stream")?;

    // Stopping: the share's drop sends on this.
    let (stop_sender, stop_receiver) = pw::channel::channel::<()>();
    let _stop = stop_receiver.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    {
        let mut stop = shared.stop.lock().unwrap();
        if stop.stopped {
            return Ok(());
        }
        stop.stream = Some(stop_sender);
    }

    let pipeline = Rc::new(RefCell::new(Pipeline {
        shared: shared.clone(),
        sink,
        layout: None,
        size: (0, 0),
        last_push: None,
        last: None,
        resizer: fast_image_resize::Resizer::new(),
    }));
    let failure: Rc<RefCell<Option<String>>> = Rc::default();
    let started = Rc::new(RefCell::new(false));

    let stream = pw::stream::StreamBox::new(
        &core,
        "langis-screen-share",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let _listener = stream
        .add_local_listener_with_user_data(pipeline.clone())
        .state_changed({
            let mainloop = mainloop.clone();
            let failure = failure.clone();
            let started = started.clone();
            move |_, _, _, new| match new {
                pw::stream::StreamState::Streaming => {
                    if !std::mem::replace(&mut *started.borrow_mut(), true) {
                        tracing::info!("screen share streaming");
                    }
                }
                pw::stream::StreamState::Error(error) => {
                    *failure.borrow_mut() = Some(error);
                    mainloop.quit();
                }
                // The desktop ended it (its indicator, or the window closed).
                pw::stream::StreamState::Unconnected => mainloop.quit(),
                _ => {}
            }
        })
        .param_changed(|_, pipeline, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let mut pipeline = pipeline.borrow_mut();
            pipeline.layout = Layout::from_spa(info.format());
            pipeline.size = (info.size().width, info.size().height);
            tracing::info!(format = ?info.format(), width = info.size().width, height = info.size().height, "screen share format");
        })
        .process(|stream, pipeline| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let (offset, size, stride) =
                (data.chunk().offset() as usize, data.chunk().size() as usize, data.chunk().stride());
            // An empty chunk carries only the pointer's movement.
            if size == 0 || stride <= 0 {
                return;
            }
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(offset..offset.saturating_add(size)) else { return };
            pipeline.borrow_mut().frame(bytes, stride as usize);
        })
        .register()?;

    let format = format_param()?;
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or_else(|| anyhow!("bad format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // The repeat timer; it also tells the starter once the first frame is in.
    let timer = mainloop.loop_().add_timer({
        let pipeline = pipeline.clone();
        move |_| pipeline.borrow_mut().repeat()
    });
    let _ = timer.update_timer(Some(Duration::from_millis(250)), Some(Duration::from_millis(250)));

    // The user picked something: it has started, as far as the call goes.
    events(ShareEvent::Started);

    mainloop.run();
    let _ = stream.disconnect();
    match failure.borrow_mut().take() {
        Some(error) => Err(anyhow!("the shared stream failed: {error}")),
        None => Ok(()),
    }
}

/// What we ask PipeWire for: raw video in one of [`Layout`]'s formats, any
/// size, any rate. Without a DMA-BUF modifier the compositor sends frames
/// in shared memory we can read.
fn format_param() -> anyhow::Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
        // xdg-desktop-portal-wlr paces itself by this one: without it, it
        // negotiates 0/0 and never sends a frame.
        spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|e| anyhow!("{e:?}"))?;
    Ok(cursor.into_inner())
}

/// A frame's rows, without their padding, as RGBA. `None` when the bytes
/// are too few for the size.
fn to_rgba(layout: Layout, width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || stride < width * 4 || bytes.len() < stride * (height - 1) + width * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let row = &bytes[row * stride..row * stride + width * 4];
        match layout {
            Layout::Rgba => rgba.extend_from_slice(row),
            Layout::Rgbx => {
                for px in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            Layout::Bgra => {
                for px in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            Layout::Bgrx => {
                for px in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
        }
    }
    Some(rgba)
}

/// The size to send: at most `max_height` tall (0: no limit) and within
/// [`MAX_SIZE`], keeping the shape; even sizes, as encoders like.
fn fit(width: u32, height: u32, max_height: u32) -> (u32, u32) {
    let (width, height) = (width.max(1) as f64, height.max(1) as f64);
    let mut scale = (MAX_SIZE.0 as f64 / width).min(MAX_SIZE.1 as f64 / height).min(1.);
    if max_height > 0 {
        scale = scale.min(max_height as f64 / height);
    }
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(width * scale), even(height * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole path on a real desktop: the portal's picker, then frames
    /// from PipeWire. Needs a session with a ScreenCast portal that picks
    /// without asking (xdg-desktop-portal-wlr with `chooser_type=none`, say).
    #[test]
    #[ignore = "needs a desktop session with a ScreenCast portal"]
    fn a_share_delivers_frames_from_the_portal() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            quality: Mutex::new(ShareQuality { max_height: 720, fps: 30 }),
            stop: Mutex::default(),
        });
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        shared.stop.lock().unwrap().portal = Some(cancel);
        let sink: Sink = Box::new(move |w, h, rgba: &[u8]| {
            let _ = tx.send((w, h, rgba.len()));
        });
        let thread_shared = shared.clone();
        let thread = std::thread::spawn(move || {
            let events = move |e| {
                let _ = events_tx.send(e);
            };
            run(&thread_shared, cancelled, sink, &events)
        });
        match events_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(event) => assert_eq!(event, ShareEvent::Started),
            Err(_) => panic!("no start: {:?}", thread.join().unwrap()),
        }
        let mut frames = Vec::new();
        while frames.len() < 3 {
            frames.push(rx.recv_timeout(Duration::from_secs(10)).expect("no frames"));
        }
        for (w, h, len) in &frames {
            assert!(*h <= 720 && *w > 0, "{w}x{h}");
            assert_eq!(*len, (*w * *h * 4) as usize);
        }
        eprintln!("frames: {frames:?}");
        // Stopping ends the stream's loop.
        let mut stop = shared.stop.lock().unwrap();
        stop.stopped = true;
        let _ = stop.stream.take().unwrap().send(());
        drop(stop);
        thread.join().unwrap().unwrap();
    }

    #[test]
    fn frames_fit_the_quality() {
        assert_eq!(fit(3840, 2160, 1080), (1920, 1080));
        assert_eq!(fit(2560, 1600, 1080), (1728, 1080));
        assert_eq!(fit(1280, 720, 1080), (1280, 720));
        // Full resolution still stays within what receivers take.
        assert_eq!(fit(7680, 4320, 0), (3840, 2160));
        assert_eq!(fit(1281, 801, 0), (1280, 800));
        assert_eq!(fit(1, 1, 0), (2, 2));
    }

    #[test]
    fn quality_settings_round_trip_and_clamp() {
        for (_, quality) in ShareQuality::PRESETS {
            assert_eq!(ShareQuality::from_setting(&quality.to_setting()), Some(quality));
        }
        assert_eq!(ShareQuality::from_setting("100@0"), Some(ShareQuality { max_height: 144, fps: 1 }));
        assert_eq!(ShareQuality::from_setting("9999@500"), Some(ShareQuality { max_height: 2160, fps: 60 }));
        assert_eq!(ShareQuality::from_setting("0@15"), Some(ShareQuality { max_height: 0, fps: 15 }));
        assert_eq!(ShareQuality::from_setting("junk"), None);
        assert_eq!(ShareQuality::default(), ShareQuality::STANDARD);
    }

    #[test]
    fn pixels_lose_their_padding_and_become_rgba() {
        // Two pixels a row, a stride of 12 (4 bytes of padding), two rows.
        let bgrx = [1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, 7, 8, 9, 0, 10, 11, 12, 0];
        let rgba = to_rgba(Layout::Bgrx, 2, 2, 12, &bgrx).unwrap();
        assert_eq!(rgba, [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]);
        let bgra = to_rgba(Layout::Bgra, 1, 1, 4, &[1, 2, 3, 4]).unwrap();
        assert_eq!(bgra, [3, 2, 1, 4]);
        assert_eq!(to_rgba(Layout::Rgbx, 1, 1, 4, &[1, 2, 3, 0]).unwrap(), [1, 2, 3, 255]);
        assert_eq!(to_rgba(Layout::Rgba, 1, 1, 4, &[1, 2, 3, 4]).unwrap(), [1, 2, 3, 4]);
        // Too few bytes, or a stride shorter than a row.
        assert!(to_rgba(Layout::Bgrx, 2, 2, 12, &bgrx[..19]).is_none());
        assert!(to_rgba(Layout::Bgrx, 2, 1, 4, &bgrx).is_none());
    }
}
