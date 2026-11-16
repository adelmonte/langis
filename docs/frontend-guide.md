# Writing a Langis frontend

A frontend does three things:

1. **Sends `Command`s** to ask for things (send a message, mark a chat read,
   start a call).
2. **Receives `Event`s** that say what changed.
3. **Reads the store** (`langis_store::AppDb`) to get what to draw.

It never talks to Signal directly. The backend runs on its own threads with
its own Tokio runtime, so the frontend can use any event loop: egui's frame
loop, GLib's main loop, Qt's, or plain `block_on`.

`crates/core/examples/demo_frontend.rs` does all of this in about 80 lines.

## 1. Starting

```rust
use langis_core::{Backend, Config};

let config = Config {
    data_dir,              // e.g. ~/.local/share/<your-app>
    db_key,                // from keys::load_or_create; see "The database key"
    device_name: "My Langis app".into(),  // shown in the phone's Linked devices
    demo: false,           // true: made-up data, no network
    former_data_dirs: Vec::new(),
    call_lines: Default::default(),
};
let (backend, events) = Backend::start(config)?;
```

`Backend` is cheap to clone; hand a copy to whatever needs it. `events` is a
`futures::channel::mpsc::UnboundedReceiver<Event>`.

One backend is one account. To run several accounts, start several backends
with separate data directories and share one `CallLines` between their
configs, so a call on one makes the others answer "busy".

## 2. The database key

Both databases (`signal.db` with the protocol keys, `app.db` with your
messages) are SQLCipher-encrypted with `db_key`. Whoever has the key and the
files can read everything. `langis_core::keys` keeps it for you:

```rust
use langis_core::keys::{self, KeySource};

let (db_key, kept_in) = keys::load_or_create(&data_dir, &KeySource::File)?;
```

| `KeySource` | Where the key is | Trade-off |
|---|---|---|
| `File` (default) | `database.key` in the data directory, mode 0600 | What Signal Desktop does without a keyring. Any program running as the user can read it. |
| `Passphrase(p)` | Nowhere: derived with Argon2id each start | The user types it every launch. Forgotten means the local history is gone. |
| `Keyring { service }` | The Secret Service (GNOME Keyring, KWallet, KeePassXC) | Needs the `keyring` cargo feature and a running keyring. |
| `Auto { service }` | The keyring if one answers, else the file | What Signal Desktop does. Needs the `keyring` feature. |

The choice is made when the data directory is first set up. Afterwards the
key must come from the same place: `load_or_create` refuses to make a new
key for databases that exist. Call it before `Backend::start`, outside any
async runtime.

## 3. Linking

A fresh data directory has no account. The backend says so, and you start
linking:

| You get | You do |
|---|---|
| `Event::NeedsLinking` | Show a link screen, and send `Command::Link`. |
| `Event::LinkUrl(url)` | Draw `url` as a QR code. The user scans it in Signal on their phone (Settings, Linked devices). |
| `Event::LinkFailed(why)` | Show `why`; send `Command::Link` again to retry. |
| `Event::Ready { aci, phone }` | Linked. Show the chat list. |

On later starts, `Ready` comes straight away. After linking, the phone sends
recent history: `Event::History(progress)` reports how far along it is.
`Command::SkipHistory` skips it.

`Event::Unlinked` means this device is no longer linked: the phone removed
it, or you sent `Command::Unlink`. The backend has already wiped the local
account and history; show the link screen again.

## 4. Connection state

`Event::Connection(state)` reports `Offline`, `Connecting`, `Syncing`
(catching up on messages that queued while offline) and `Online`. While
syncing, `Event::CatchingUp { first, latest }` gives timestamps for a
progress bar.

After the machine wakes from sleep, send `Command::Resumed` so dead
connections are noticed at once rather than minutes later.

## 5. Reading state

Store queries are async and run on the backend's runtime. `Backend::spawn`
runs one there and gives back a future you can await from any executor:

```rust
let db = backend.db().clone();
let chats = backend.spawn(async move { db.conversations().await }).await?;
```

With no async runtime of your own, wrap it in `futures::executor::block_on`.
Don't block a UI thread on it for long: start the query, and pick up the
result on a later frame or in a GLib/Qt callback.

The reads you will use most:

| Query | Gives |
|---|---|
| `conversations()` | The chat list, pinned first, newest first. `archived_conversations()` for the rest. |
| `conversation(&id)` | One chat. |
| `messages(&id, before, limit)` | A page of messages, newest first. Pass the oldest `id` you have as `before` to page back. |
| `messages_around`, `messages_after` | Pages around a search hit or after a point. |
| `message(id)` | One message, after `MessageChanged` names it. |
| `display_names()` | Names for authors and mentions. |
| `search_messages`, `search_conversation`, `search_directory` | Search. |
| `setting(key)` | Account settings (`langis_core::SETTING_*` keys). |

Rule of thumb: **read the store freely, change it through `Command`s.** The
backend keeps the store, Signal's servers and your other devices in step,
and a direct write skips all of that. The exceptions are frontend-only
state: drafts (`set_draft`, `set_draft_with_styles`), emoji use for a recent
list (`record_emoji_use`), and your own preferences in `set_setting` under a
prefix of your own (for example `ui.`).

## 6. Events say what to re-read

Most events carry ids, not data. React by re-reading:

| Event | Re-read |
|---|---|
| `ConversationsChanged` | `conversations()` |
| `MessageChanged { conversation, message_id }` | `message(message_id)`, if that chat is open |
| `Incoming { conversation, message_id, mentions_me, replies_to_me }` | Same, and notify the user if you want to |
| `MessageRemoved`, `ConversationRemoved`, `ConversationCleared` | Drop it from your view |
| `Typing { conversation, author, started }` | Show or hide "typing..." |
| `AccountSettingsChanged` | `setting(...)` for what you show |
| `Error(text)` | Show it |

Commands that ask a question answer with their own event, named in the
command's docs (for example `Command::LoadSafetyNumber` answers with
`Event::SafetyNumber`).

## 7. Sending

```rust
backend.send(Command::SendText {
    conversation: chat_id,
    body: "Hello".into(),
    quote: None,          // Some(message id) to reply
    preview: None,        // a link preview from Command::FetchLinkPreview
    mentions: Vec::new(),
    styles: Vec::new(),   // bold, italic, spoiler, ...
});
```

The message appears in the store at once with `status: Sending`, and each
change (sent, delivered, read, failed) comes as `MessageChanged`. Files go
with `SendImages` or `SendFile`, and replies, reactions, edits and deletes
each have a command.

**Safety numbers.** If the user marked someone verified and their safety
number changes, sends to them are held until the user confirms, as in
Signal Desktop. The core enforces it: a send that would use a verified
contact's new key fails for them (`SEND_ERROR_IDENTITY` in the message's
send states) and the "safety number changed" notice goes in the chat.

Your part is the prompt. Before a send, call
`langis_core::unverified_people(db)` and `stopping_send(&conversation, &set)`;
if anyone is listed (or a send failed that way), show who and offer
"Send anyway". That's `Command::SetVerified { conversation, verified: false }`
for each of them, then `Command::RetrySend` for a failed message.

Mark a chat read when the user looks at it: `Command::MarkRead { conversation }`.
That also tells the phone and, with read receipts on, the sender.

## 8. Attachments

A message's `attachments` list each file's type, size and dimensions.
`local_path` is set once the file is downloaded and decrypted. By default
everything downloads on its own; `Command::SetAutoDownload` turns that off
per kind (photos, videos, audio, documents), and stickers and short voice
messages always come. Anything not downloaded waits for
`Command::FetchAttachment { message_id, index }`. `MessageChanged` follows
when the file is ready.

## 9. Calls

| You do | You get |
|---|---|
| `Command::StartCall { conversation, video }` | `Event::Call(Some(update))` as it rings and connects |
| `Command::AcceptCall { video }` / `DeclineCall` / `HangUp` | `Event::Call(None)` when it's over |
| `Command::SetCallAudio(on)`, `SetCallVideo(on)` | Updated `Event::Call` |
| `Command::LoadCallDevices`, `SetCallDevice` | `Event::CallDevices` |

An incoming call arrives as `Event::Call(Some(update))` with
`update.incoming` set: ring.

Video doesn't go through events. `backend.call_video()` holds the latest
frames: `remote`, `local` (your camera, mirrored) and `group` (one per
participant). Each `FrameSlot::latest()` returns a `Frame` with `width`,
`height`, BGRA pixels and a `seq` number. Poll it each frame you draw, and
upload the pixels to a texture only when `seq` changed.

Screen sharing isn't available on Linux yet: `Command::LoadShareSources`
answers with an empty list.

## 10. Stopping

Call `backend.stop(Duration::from_secs(5))` from a non-async thread before
the process exits. It finishes the work in hand and closes the databases
cleanly.

## Demo mode

`demo: true` seeds made-up chats, people, groups, stickers and media, and
answers sends locally. Nothing touches the network except link previews you
type and, with a Giphy key, GIF search. Use it to build and screenshot your
interface without an account.

Always give demo mode its own scratch `data_dir`. It writes into whatever
directory it gets, a real account's included.
