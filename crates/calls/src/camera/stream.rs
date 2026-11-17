//! Cameras through PipeWire, as Chromium's WebRTC takes them on Linux
//! (`WebRtcPipeWireCamera`). WirePlumber offers every camera there as a
//! `Video/Source` node: USB webcams through V4L2, and laptop MIPI sensors
//! (Intel IPU6 and IPU7) through libcamera, which turns their raw sensor
//! data into pictures. Those sensors' `/dev/video*` nodes give only the raw
//! data, which V4L2 capture can't use.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use pipewire as pw;
use pw::spa;
use ringrtc::webrtc::media::{VideoFrame, VideoPixelFormat, VideoSource};

use crate::FrameSlot;

/// How long a started camera may take to send its first picture.
const FIRST_FRAME: Duration = Duration::from_secs(10);

/// A camera PipeWire offers.
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub id: u32,
    pub serial: Option<String>,
    pub name: String,
    /// WirePlumber's `priority.session`: the highest is its default.
    pub priority: i64,
}

/// The cameras PipeWire offers, its default first. Empty without
/// PipeWire.
pub(super) fn cameras() -> Vec<Node> {
    list().unwrap_or_else(|error| {
        tracing::debug!(error = %format!("{error:#}"), "no PipeWire cameras");
        Vec::new()
    })
}

fn list() -> anyhow::Result<Vec<Node>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("starting PipeWire")?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let registry = core.get_registry()?;
    let found: Rc<RefCell<Vec<Node>>> = Rc::default();
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0)?;
    let _core = core
        .add_listener_local()
        .done({
            let (done, mainloop) = (done.clone(), mainloop.clone());
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    let _registry = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else { return };
                if props.get("media.class") != Some("Video/Source") || props.get("media.role").is_some_and(|r| r != "Camera") {
                    return;
                }
                let name = props.get("node.description").or(props.get("node.nick")).or(props.get("node.name")).unwrap_or("Camera");
                found.borrow_mut().push(Node {
                    id: global.id,
                    serial: props.get("object.serial").map(str::to_owned),
                    name: name.to_owned(),
                    priority: props.get("priority.session").and_then(|p| p.parse().ok()).unwrap_or(0),
                });
            }
        })
        .register();
    let timeout = mainloop.loop_().add_timer({
        let (done, mainloop) = (done.clone(), mainloop.clone());
        move |_| {
            done.set(true);
            mainloop.quit();
        }
    });
    let _ = timeout.update_timer(Some(Duration::from_secs(2)), None);
    while !done.get() {
        mainloop.run();
    }
    let mut cameras = found.take();
    cameras.sort_by_key(|c| std::cmp::Reverse(c.priority));
    Ok(cameras)
}

/// The pixel formats we take, in the order we prefer them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Rgbx,
    Bgrx,
    Rgba,
    Bgra,
    Rgb,
    Bgr,
    Yuy2,
    Nv12,
}

impl Layout {
    fn from_spa(format: spa::param::video::VideoFormat) -> Option<Self> {
        use spa::param::video::VideoFormat as F;
        Some(match format {
            F::RGBx => Self::Rgbx,
            F::BGRx => Self::Bgrx,
            F::RGBA => Self::Rgba,
            F::BGRA => Self::Bgra,
            F::RGB => Self::Rgb,
            F::BGR => Self::Bgr,
            F::YUY2 => Self::Yuy2,
            F::NV12 => Self::Nv12,
            _ => return None,
        })
    }
}

struct Pipeline {
    source: VideoSource,
    preview: FrameSlot,
    layout: Option<Layout>,
    size: (u32, u32),
    frames: u64,
}

/// Streams camera `node` into the call and the self-view until `stop`.
pub(super) fn run(stop: &Arc<AtomicBool>, source: &VideoSource, preview: &FrameSlot, node: &Node) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("starting PipeWire")?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let failure: Rc<RefCell<Option<String>>> = Rc::default();
    let pipeline = Rc::new(RefCell::new(Pipeline { source: source.clone(), preview: preview.clone(), layout: None, size: (0, 0), frames: 0 }));

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
    };
    if let Some(serial) = &node.serial {
        props.insert("target.object", serial.as_str());
    }
    let stream = pw::stream::StreamBox::new(&core, "langis-camera", props)?;
    let _listener = stream
        .add_local_listener_with_user_data(pipeline.clone())
        .state_changed({
            let (mainloop, failure) = (mainloop.clone(), failure.clone());
            move |_, _, _, new| match new {
                pw::stream::StreamState::Error(error) => {
                    *failure.borrow_mut() = Some(error);
                    mainloop.quit();
                }
                pw::stream::StreamState::Unconnected => {
                    failure.borrow_mut().get_or_insert_with(|| "the camera went away".into());
                    mainloop.quit();
                }
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
            tracing::info!(format = ?info.format(), width = info.size().width, height = info.size().height, "camera format");
        })
        .process(|stream, pipeline| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let mut pipeline = pipeline.borrow_mut();
            let (Some(layout), (width, height)) = (pipeline.layout, pipeline.size) else { return };
            let datas = buffer.datas_mut();
            let mut planes: Vec<(Vec<u8>, usize)> = Vec::with_capacity(2);
            for data in datas.iter_mut().take(2) {
                let (offset, size, stride) = (data.chunk().offset() as usize, data.chunk().size() as usize, data.chunk().stride());
                let Some(bytes) = data.data() else { break };
                let Some(bytes) = bytes.get(offset..offset.saturating_add(size)) else { break };
                planes.push((bytes.to_vec(), stride.max(0) as usize));
            }
            let Some((first, stride)) = planes.first() else { return };
            let second = planes.get(1).map(|(bytes, stride)| (bytes.as_slice(), *stride));
            let Some(rgba) = to_rgba(layout, width as usize, height as usize, first, *stride, second) else { return };
            pipeline.source.push_frame(VideoFrame::copy_from_slice(width, height, VideoPixelFormat::Rgba, &rgba));
            pipeline.preview.put_rgba(width, height, &rgba, true);
            pipeline.frames += 1;
            if pipeline.frames == 1 {
                tracing::info!("camera started");
            }
        })
        .register()?;

    let format = format_param()?;
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or_else(|| anyhow!("bad format"))?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node.id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // Stopping, and a camera that never sends a picture.
    let started = Instant::now();
    let timer = mainloop.loop_().add_timer({
        let (mainloop, failure, pipeline, stop) = (mainloop.clone(), failure.clone(), pipeline.clone(), stop.clone());
        move |_| {
            if stop.load(Ordering::Relaxed) {
                mainloop.quit();
            } else if pipeline.borrow().frames == 0 && started.elapsed() > FIRST_FRAME {
                *failure.borrow_mut() = Some("the camera sent no picture".into());
                mainloop.quit();
            }
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100)));
    mainloop.run();
    let _ = stream.disconnect();
    match failure.take() {
        Some(error) if !stop.load(Ordering::Relaxed) => Err(anyhow!("{error}")),
        _ => Ok(()),
    }
}

/// Raw video in one of [`Layout`]'s formats, 720p at 30 frames a second
/// preferred (as Desktop sends at most), any size and rate allowed.
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
            VideoFormat::RGBx,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::BGRA,
            VideoFormat::RGB,
            VideoFormat::BGR,
            VideoFormat::YUY2,
            VideoFormat::NV12
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1280, height: 720 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 4096, height: 4096 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 120, denom: 1 }
        ),
    );
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(cursor.into_inner())
}

/// BT.601 limited range, as webcams send.
fn yuv(y: u8, u: u8, v: u8) -> [u8; 4] {
    let (c, d, e) = (y as i32 - 16, u as i32 - 128, v as i32 - 128);
    let clamp = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [clamp(298 * c + 409 * e), clamp(298 * c - 100 * d - 208 * e), clamp(298 * c + 516 * d), 255]
}

/// A frame as RGBA, without row padding. `second` is NV12's chroma plane
/// when it comes separately; otherwise it follows the luma. `None` when
/// the bytes are too few for the size.
fn to_rgba(layout: Layout, width: usize, height: usize, bytes: &[u8], stride: usize, second: Option<(&[u8], usize)>) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = match layout {
        Layout::Rgbx | Layout::Bgrx | Layout::Rgba | Layout::Bgra => width * 4,
        Layout::Rgb | Layout::Bgr => width * 3,
        Layout::Yuy2 => width.div_ceil(2) * 4,
        Layout::Nv12 => width,
    };
    let stride = if stride == 0 { row_bytes } else { stride };
    if stride < row_bytes || bytes.len() < stride * (height - 1) + row_bytes {
        return None;
    }
    let mut rgba = Vec::with_capacity(width * height * 4);
    if layout == Layout::Nv12 {
        let (chroma, chroma_stride) = match second {
            Some(plane) => plane,
            None => (bytes.get(stride * height..)?, stride),
        };
        let chroma_stride = if chroma_stride == 0 { stride } else { chroma_stride };
        if chroma.len() < chroma_stride * (height.div_ceil(2) - 1) + width.div_ceil(2) * 2 {
            return None;
        }
        for row in 0..height {
            let luma = &bytes[row * stride..row * stride + width];
            let uv = &chroma[(row / 2) * chroma_stride..];
            for (x, y) in luma.iter().enumerate() {
                rgba.extend_from_slice(&yuv(*y, uv[(x / 2) * 2], uv[(x / 2) * 2 + 1]));
            }
        }
        return Some(rgba);
    }
    for row in 0..height {
        let row = &bytes[row * stride..row * stride + row_bytes];
        match layout {
            Layout::Rgba => rgba.extend_from_slice(row),
            Layout::Rgbx => row.as_chunks::<4>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            Layout::Bgra => row.as_chunks::<4>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]])),
            Layout::Bgrx => row.as_chunks::<4>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[2], p[1], p[0], 255])),
            Layout::Rgb => row.as_chunks::<3>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            Layout::Bgr => row.as_chunks::<3>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[2], p[1], p[0], 255])),
            Layout::Yuy2 => {
                for (i, p) in row.as_chunks::<4>().0.iter().enumerate() {
                    rgba.extend_from_slice(&yuv(p[0], p[1], p[3]));
                    if i * 2 + 1 < width {
                        rgba.extend_from_slice(&yuv(p[2], p[1], p[3]));
                    }
                }
            }
            Layout::Nv12 => unreachable!(),
        }
    }
    Some(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_convert_to_rgba() {
        // Two pixels of 24-bit BGR with a padded row.
        let bgr = [0, 0, 255, 0, 255, 0, 9, 9];
        assert_eq!(to_rgba(Layout::Bgr, 2, 1, &bgr, 8, None), Some(vec![255, 0, 0, 255, 0, 255, 0, 255]));
        // YUY2 white and black share a chroma pair.
        let yuy2 = [235, 128, 16, 128];
        assert_eq!(to_rgba(Layout::Yuy2, 2, 1, &yuy2, 4, None), Some(vec![255, 255, 255, 255, 0, 0, 0, 255]));
        // NV12 2x2 gray, chroma after the luma.
        let nv12 = [126, 126, 126, 126, 128, 128];
        let gray = to_rgba(Layout::Nv12, 2, 2, &nv12, 2, None).unwrap();
        assert!(gray.chunks(4).all(|p| p == [128, 128, 128, 255]), "{gray:?}");
        // Too few bytes.
        assert_eq!(to_rgba(Layout::Rgba, 2, 2, &[0; 8], 8, None), None);
    }
}

