# langis

A Signal client library in Rust, with no UI. langis does the Signal side
(linking, sending, receiving, groups, attachments, calls, sync with your
other devices) and stores everything in an encrypted database. You bring the
interface: egui, GTK, Qt, a TUI, a bot, whatever you like.

It links to your phone as a secondary device, the way Signal Desktop does.
It is unofficial and not affiliated with Signal.

langis is a fork of [Vernam](https://github.com/knpwrs/vernam), a native
Signal client for macOS. It keeps Vernam's backend, drops the macOS app, and
targets Linux.

## Status

Early, but in daily use. What has been checked so far, on Linux:

- Linking to a real phone as a secondary device, and everyday messaging
  against Signal's servers, through [langis-gui](https://github.com/adelmonte/langis-gui)
  since November 2026.
- The workspace builds, and its 500+ tests pass (`cargo test --workspace`),
  the vendored libraries' own included.
- The demo backend runs end to end: `cargo run -p langis-core --example demo_frontend`.
- Calls on Linux, in real use (audio goes through WebRTC's PulseAudio
  support, which PipeWire also serves; the camera through PipeWire, which
  covers laptop MIPI cameras through libcamera, else V4L2).
- A code review for security (November 2026) of the core, the calls code,
  Storage Service sync, key transparency and every vendored change. Its
  findings are fixed; see `git log`. It was a code reading by AI reviewers,
  not a professional audit.

Not checked yet:

- Screen sharing in a real call. The capture half (the desktop portal's
  picker, then frames over PipeWire, scaled to the chosen quality) passes
  `scripts/screen-share-test.sh`, which runs it in a throwaway headless
  sway session. GNOME's and KDE's portals haven't been tried.

Videos are re-encoded smaller before sending, as Desktop does, when
`ffmpeg` (with libx264) is installed; without it they go as they are.

## Crates

| Crate | What it does |
|---|---|
| `langis-core` | The backend a frontend drives: `Backend::start`, `Command`s in, `Event`s out. |
| `langis-store` | The encrypted app database (`AppDb`) a frontend reads chats and messages from. |
| `langis-calls` | Calls through Signal's RingRTC. Used by the core; frontends only see its video frames. |
| `langis-emoji` | Emoji data and search, for frontends that want a picker. |
| `vendor/` | Modified copies of presage and libsignal-service-rs. |

## Frontends

- [langis-gui](https://github.com/adelmonte/langis-gui): a desktop client
  in egui that follows Signal Desktop's look and layout.

## Writing a frontend

Start with [docs/frontend-guide.md](docs/frontend-guide.md), then read
[crates/core/examples/demo_frontend.rs](crates/core/examples/demo_frontend.rs).
The full API reference is `cargo doc -p langis-core -p langis-store --open`.

In short:

```rust
let (backend, mut events) = Backend::start(config)?;     // starts its own threads
backend.send(Command::Link);                              // ask for things
while let Some(event) = events.next().await {             // hear back
    match event {
        Event::LinkUrl(url) => show_qr_code(url),
        Event::ConversationsChanged => reload_chat_list(&backend),
        _ => {}
    }
}
```

## Building

Rust 1.94 or newer, and:

- `protoc` (Arch: `protobuf`, Debian/Ubuntu: `protobuf-compiler`)
- `clang` (for BoringSSL's bindings in libsignal)
- `cmake`
- PulseAudio headers (Arch: `libpulse`, Debian/Ubuntu: `libpulse-dev`)
- PipeWire headers, for screen sharing (Arch: `libpipewire`,
  Debian/Ubuntu: `libpipewire-0.3-dev`)

At run time, optionally: `ffmpeg` to shrink videos before sending, and an
XDG desktop portal with ScreenCast (most desktops have one) to share the
screen in calls.

The first build downloads Signal's prebuilt WebRTC (about 28 MB) into
`target/webrtc`, and takes a few minutes.

```sh
cargo test --workspace
cargo run -p langis-core --example demo_frontend
```

## License

AGPL-3.0-only, like Signal's libraries it links. See LICENSE and NOTICE.
A frontend that links langis is under the AGPL too.
