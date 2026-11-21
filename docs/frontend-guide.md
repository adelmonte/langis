# Writing a langis frontend

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
    device_name: "My langis app".into(),  // shown in the phone's Linked devices
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

One backend per data directory: `Backend::start` takes `langis.lock` in it
and fails with "already in use by another running copy" while another
process (or another `Backend` in this one) has it. Show that, or bring the
running window forward, rather than starting a second copy.

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
| `Event::Linked` | The phone scanned the code. Show a "Syncing contacts and groups" step until `Ready`. |
| `Event::LinkFailed { error, detail }` | Show the failure by its `LinkError` kind (below); send `Command::Link` again to retry. `detail` is the error's text, for logs. |
| `Event::LinkedOtherAccount` | The phone linked another account than the one whose data is here. Ask as Desktop does ("Delete all data?"), then send `Command::ContinueLink { delete_data }`: `true` deletes the old data and goes on to `Linked`; `false` drops the new link and gives `NeedsLinking` for a new code. |
| `Event::Ready { aci, phone }` | Linked. Show the chat list. |

`LinkError` sorts failures as Signal Desktop's install screen does
(`ts/state/ducks/installer.preload`): `Timeout` (no code came; Desktop
shows "The QR code couldn't load" and Retry in the code's box), `Expired`
(the code went unscanned until the server closed it; Desktop shows only
"Refresh code"), `Unknown` (anything else once the code showed; "An
unexpected error occurred", Retry), and three for a full error screen:
`ConnectionFailed` (the server couldn't be reached for a code),
`TooManyDevices` (HTTP 411) and `TooOld` (HTTP 409, the server wants a
newer client).

Whose data is here is told by Note to Self's id in the app database. A
link to the same account, or to a data directory with no account yet, goes
straight on. `LinkedOtherAccount` can also come at a start, when the app
closed while the question was open: nothing of the new account runs, and
nothing is deleted, until the answer. Linking again with `Command::Link`
also drops the waiting link. Unlike Desktop, which asks before it
registers, langis asks after the server registered the device, so "no"
unlinks it from the phone again.

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
| `messages(&id, before, limit)` | A page of messages, oldest first (the newest at the end). Pass the oldest `id` you have as `before` to page back. |
| `messages_around`, `messages_after` | Pages around a search hit or after a point. |
| `media_messages`, `media_messages_before` | A chat's photos, videos, files and links, newest first, a page at a time. |
| `message(id)` | One message, after `MessageChanged` names it. |
| `display_names()` | Names for authors and mentions. |
| `search_messages`, `search_conversation`, `search_directory` | Search. |
| `setting(key)` | Account settings (`langis_core::SETTING_*` keys). |

Rule of thumb: **read the store freely, change it through `Command`s.** The
backend keeps the store, Signal's servers and your other devices in step,
and a direct write skips all of that. The exceptions are frontend-only
state: drafts (`set_draft`, `set_draft_with_styles`), emoji use for a recent
list (`record_emoji_use`), and your own preferences in `set_setting` under a
prefix of your own (for example `ui.`). The theme, the two local call
settings (incoming calls, always relay), image optimization and the Giphy
key are the core's keys: change them with `Command::SetLocalSetting`, and
read the theme back with `Theme::from_setting(db.setting(SETTING_THEME))`.

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
| `ChatColorsChanged` (after `Command::EditChatColors`) | `chat_colors()` |
| `NotificationProfilesChanged` | `notification_profile_state(now)`, `notification_profiles()` |
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
each have a command. `SendGif` sends a GIF from `SearchGifs` with an optional caption (mentions
and formatting as `SendFile`'s). `Forward { message_ids, conversations, body }` copies
messages to other chats; `body` is a single message's text as edited
before sending (Desktop's forward editor), `None` to send it unchanged.

Chat state changes go through `Command::SetChat { conversation, change }`
(archive, mute, mark unread, block, hide stories, and a group's
`DontNotifyMentions`, Desktop's "Mentions: Don't notify" while muted),
here at once and on other devices through Storage Service. A
`Conversation` carries them back, with `blocked_at` (when it was blocked)
and a person's profile `about`.

**Repeated requests to join.** A request to join by a group's link and
its withdrawal by the requester fold into the chat's latest notice when it
is about the same person: `GroupNotice::RequestBounce { member, times,
pending }` (Desktop's "requested and canceled 2 requests", followed by
"requested to join" while `pending`). An admin's "Block request" on it is
`Command::BlockGroupLinkRequests { conversation, member }`: they're banned
from the link (into `group_state.banned`). Desktop shows the button to
admins while they're neither a member nor banned.

**No longer on Signal.** A send that finds someone unregistered (the
server's 404) sets their chat's `unregistered_at`, as Desktop's
`discoveredUnregisteredAt`; a send that reaches them, or a message from
them, clears it. `Conversation::ever_unregistered()` is Desktop's test for
replacing the composer with "This person isn't using Signal" and hiding the
call buttons; `unregistered(now)` (found in the last six hours) is its test
for leaving them out of the people offered for a new chat or group.

**Removing a person.** `ChatChange::Removed(true)` is Desktop's "Remove" on
a contact row (`removeContact`): their chat's `removal_stage` becomes
`JustNotification`, our profile is no longer shared with them, the chat
gets a `Notice::ContactRemoved`, and Storage Service's `hidden` is set.
Leave them out of the people offered for a new chat. The chat isn't
`accepted` while removed, but with `JustNotification` it keeps its composer
(Desktop shows message request actions only for `MessageRequest`, which it
becomes when they message us, worded "You have removed this person in the
past"). Accepting, sending them a message or `Removed(false)` restores
them.

`ChatChange::Notify(kind, value)` sets one of Desktop's per-chat "While
muted" switches (calls; mentions and replies for groups) or its unread
reminders, kept in `Conversation::notify` (`ChatNotify`). `None` follows
the account setting `AccountSetting::for_chat(kind)`; use
`ChatNotify::on(kind, ...)` with the account settings to decide. Mentions
off also sets `dont_notify_mentions`. The core rings for a call in a
muted chat only when calls are on there; message notifications are yours:
for a muted chat, notify only a group's mention (`Event::Incoming`'s
`mentions_me`) or reply to us (`replies_to_me`) whose kind is on, as
Desktop's `maybeNotify` does.

**Unread reminders.** Hourly, the core looks for Desktop's unread reminder
(`ts/services/unreadReminders.preload.ts`): a muted, unarchived chat with
unread reminders on (`ChatNotifyKind::UnreadReminders`), whose oldest
unread message since its last reminder is over three days old and newest
under fourteen, at most one reminder per chat every three days. Only
messages after the first run count. It sends `Event::UnreadReminder {
conversation, reminder }` for the chat waiting longest; `reminder`
(`ReminderData`) has the counts and, as service ids, who wrote, mentioned
us and replied to us, mention and reply counts zeroed when that kind is off
there while muted. Show it as a notification (Desktop's wording is in
`ts/util/unreadReminders.std.ts`) and answer `Command::UnreadReminderShown
{ conversation }`; until you do, it comes again each hour. Notification
profiles aren't checked.

**Call quality survey.** After a call ends, `Event::CallQualitySurvey`
(`call_survey::CallSurvey`: the chat, Desktop's call type, when it
connected and ended, whether it failed) asks for Desktop's "How was your
call?" dialog, as Desktop decides (`ts/util/callQualitySurvey.dom.ts`):
only for calls RingRTC marks a survey candidate (it connected), at most
daily, always for a failed call unless one was asked about within a day,
else for the share of calls in the remote config's
`desktop.callQualitySurveyPPM` for our number's country (1% without it).
There's no command to submit it: Desktop sends the answers to Signal's
gRPC `CallQuality` service over libsignal-net's chat connection, which
langis doesn't have, so the answers stay with the frontend.

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

**Username problems.** Once a day, after the Storage Service sync, the
core compares our username and username link with the server's (Desktop's
`usernameIntegrity`). `OwnProfile::username_corrupted` and
`username_link_corrupted` (from `own_profile`, re-read on
`Event::OwnProfileChanged`) say one no longer matches; show Desktop's
left-pane banner with "Fix now". Setting the username again
(`ConfirmUsername`), deleting it, or `ResetUsernameLink` clears them.

**Links in messages.** Draw a link as a link only when it's `http`,
`https` or `mailto` and `langis_core::link_preview::is_link_sneaky(href)`
is false (Desktop's `Linkify`); a sneaky one (credentials in it, an IP
address, mixed scripts in the domain, odd characters in the path) stays
plain text.

**Notification profiles** (Desktop's Settings > Notifications >
Notification profiles). A profile names the people and groups whose
notifications still come while it's on, and can let every call
(`allow_all_calls`) and every mention or reply (`allow_all_mentions`)
through. It turns on by a weekly schedule (`schedule_start` and
`schedule_end` as hhmm, `schedule_days` Monday first; an end at or before
the start runs into the next day) or by hand. Read them with
`AppDb::notification_profiles()` (newest first, deleted ones with
`deleted_at` set; `langis_store::current_profiles` drops those) and ask
which is on with `AppDb::notification_profile_state(now)`: the profile on,
and `next_change`, when to ask again. `AppDb::notification_silenced(chat,
is_call, mention_or_reply, now)` (or `langis_store::should_notify` with the
active profile in hand) says whether to stay quiet; Desktop also leaves
chats the profile doesn't allow out of the unread badge, as it does muted
ones.

Change them with `Command::EditNotificationProfiles`:
`NotificationProfileEdit::Save(profile)` adds or replaces one (a new one
takes `langis_core::new_notification_profile_id()` and
`NotificationProfile::new`, Desktop's defaults), `Delete { id, at }`,
`Enable { id, ends_at, at }` turns one on until `ends_at` or until its
schedule next turns it off, and `Disable { at }` turns profiles off until a
schedule next turns one on. `Event::NotificationProfilesChanged` follows,
for this device's changes and other devices' (each profile is a Storage
Service `NotificationProfile` record; the manual override is in the
account record). Desktop's "Sync across devices" switch isn't there: they
always sync.

Mark a chat read when the user looks at it: `Command::MarkRead { conversation }`.
That also tells the phone and, with read receipts on, the sender.

## 8. Attachments

A message's `attachments` list each file's type, size and dimensions.
`local_path` is set once the file is downloaded and decrypted; until
then a photo or video's `blurhash`, when the sender gave one, can be drawn
blurred in its place. By default
everything downloads on its own; `Command::SetAutoDownload` turns that off
per kind (photos, videos, audio, documents), and stickers and short voice
messages always come. Anything not downloaded waits for
`Command::FetchAttachment { message_id, index }`. `MessageChanged` follows
when the file is ready.
A link preview's image downloads as photos do; when that setting leaves it,
`Command::FetchPreviewImage` gets it. While one downloads, `Event::DownloadProgress` reports the bytes received a
few times a second, and `Command::CancelDownload` stops it (it ends with
`received` 0). Files from transferred history download without progress.
`Event::AttachmentUnavailable { message_id, index, permanent }` says one
couldn't come: gone for good (Desktop's "This media is not available"), or
worth another `FetchAttachment`.
**Backup media download.** When the account has paid media backups (the
phone's Signal Secure Backups with all media), a transferred backup's
photos, videos and files that the auto-download choices take are queued to
download from the backup tier, as Desktop's "Syncing media" does
(`ts/util/backupMediaDownload.preload.ts`). `Event::BackupMediaDownload`
(`backups::BackupMediaDownload`) says how far it is: `total_bytes` and
`completed_bytes` (each file counts its expected encrypted size; one that
failed for good counts as done), `paused`, `idle` (files are waiting for a
later try: offline, or the backup tier failing; Desktop's "Media will
continue to download in the background"), `dismissed` and `media_backups`.
It comes at a start once connected, a few times a second while files
come, and after each command; `Command::LoadBackupMediaDownload` asks for
it. `show_banner()` is Desktop's test for the left pane's banner and
`complete()` for its "Restore complete". `Command::BackupMediaDownload(action)`
takes Desktop's buttons: `Pause` and `Resume` (kept across restarts),
`Cancel` (after Desktop's "Cancel media syncing?" dialog: the queue and
counts go; the files stay downloadable one by one) and `Dismiss` (the
banner's close button). One file downloads at a time, newest first; a
file that fails is tried again on later runs (each time the message queue
drains, on resume, and five minutes after a run left some), and after three
tries or one lasting failure gets its backup thumbnail instead. Each file
still gives `MessageChanged` when it's in.

A story's photo or video comes with `Command::FetchStory { id }` (then
`StoriesChanged`); `Event::StoryDownloadFailed { id }` means it couldn't, and
another `FetchStory` tries again.
Our own story's `failed` lists who its send didn't reach (Desktop's "Send
failed" when that's everyone, "Partially sent" otherwise);
`Command::RetryStory { id }` sends it to them again.

Videos (MP4, Ogg, WebM) are re-encoded to H.264 at Desktop's sizes before
upload when `ffmpeg` with libx264 is on `PATH`; the message shows as sending
meanwhile. Without ffmpeg the original goes.

## Backups

Settings > Backups shows Signal Secure Backups as a linked Desktop does
(`PreferencesBackups.dom.tsx`, `PreferencesBackupDetails.dom.tsx`). Only the
phone turns backups on or pays for them, so the page only reports.
`Command::RefreshBackupStatus { details }` when the page opens (`details:
true` for its details page) answers with `Event::BackupStatus` at once from
what's stored, with `fetching` set when a fetch started, and again once the
server answered. Each fetch runs at most every 30 seconds.

`backups::BackupStatus` has:

| Field | Desktop shows |
|---|---|
| `tier: None` | Backups off or not set up: "Signal Secure Backups", "Automatic backups with Signal's secure, end-to-end encrypted storage service. Get started on your phone." |
| `tier: Some(Free)` with `free_media_days` | "Text + {n} days media backup", "Your backup plan is free" |
| `tier: Some(Paid)` with `subscription` | "Text + all media backup" and the subscription: `Active { cost, renews_at }` ("{cost} / month", "Renews {date}"), `PendingCancellation { cost, expires_at }` ("Subscription canceled", "Expires {date}"), `NotFound` or `Expired` ("Your subscription was not found."). `None` before the first fetch reads as not found. |
| `last_backup` (`created_at`, `size`) | "Last backup: Your phone, {date}" on the details page (Desktop fetches the size but doesn't show it) |
| `show_spinner(now)` | A spinner in place of the subscription while fetching over data older than five minutes |

`Cost::amount` is in whole units of `currency` (1.99 USD). Where it comes
from: the tier is the account record's `backupTier`; the subscription is
`GET /v1/subscription/{subscriberId}` with the subscriber id from the
account record (or the transferred backup); the last backup is the message
backup file's `Last-Modified` and `Content-Length` on the backup CDN, read
with a message backup credential made from the phone's account entropy
pool. With no entropy pool yet the last backup stays unknown. A failed
subscription fetch reads as not found, as in Desktop; a failed last-backup
fetch keeps what was there. The demo answers with a $1.99 paid plan and a
backup from last night.

## 9. Calls

| You do | You get |
|---|---|
| `Command::StartCall { conversation, video }` | `Event::Call(Some(update))` as it rings and connects |
| `Command::AcceptCall { video }` / `DeclineCall` / `HangUp` | `Event::Call(None)` when it's over |
| `Command::SetCallAudio(on)`, `SetCallVideo(on)` | Updated `Event::Call` |
| `Command::LoadCallDevices`, `SetCallDevice`, `SetShareQuality` | `Event::CallDevices` |
| `Command::PeekGroupCall { conversation }` (the lobby) | `Event::GroupPeek { users, max_devices }`: who's in it; with `users.len()` at `max_devices` the call is full |
| `Command::SetOutgoingRing(on)` (the lobby's Ring toggle) | A group call we start then rings the group (16 or fewer) or only notifies it |
| `Command::PeekGroupCall` with a call link's `ConversationId` | `Event::GroupPeek` for the link's call: who's in it now, e.g. for the link's details |
| `Command::RemoveFromCall { demux_id, block }` (a call link we administer) | They leave the call's `participants`; with `block` they can't come back to this call |

An incoming call arrives as `Event::Call(Some(update))` with
`update.incoming` set: ring.

In a group call, `update.participants` lists the others, one entry per
device. Each has `video`, `audio` and `presenting`, and `speaker_time`
(when they were last the main speaker, Unix milliseconds) for ordering
tiles as Desktop does: presenters first, then the most recent speaker.
`media_keys` is false while we lack their media keys, so can't decrypt
them; Desktop's tile says "Can't receive audio and video from {name}" once
that has lasted 5 seconds past the later of their `added_at` and our join.

Video doesn't go through events. `backend.call_video()` holds the latest
frames: `remote`, `local` (your camera, mirrored) and `group` (one per
participant). Each `FrameSlot::latest()` returns a `Frame` with `width`,
`height`, BGRA pixels and a `seq` number. Poll it each frame you draw, and
upload the pixels to a texture only when `seq` changed.

`call_video().levels` holds a group call's audio levels, refreshed every
200ms: `level(demux_id)` for a participant and `local()` for us, each 0 to
1. Poll it as you draw, for speaking indicators.

### Screen sharing

Sharing goes through the XDG desktop portal, so the desktop (GNOME, KDE,
wlroots compositors like sway, niri through its portal) shows its own
picker for the screen or window. The flow:

1. `Command::LoadShareSources` answers with `Event::ShareSources`: one
   source, `langis_core::PORTAL_SOURCE`, when the desktop has a ScreenCast
   portal, and an empty list when it doesn't (hide the Present button).
2. `Command::StartSharing { source }` opens the desktop's picker. Nothing
   changes in the call yet. While already presenting it's ignored.
3. When the user picks something and its first frame goes out (the
   screen has to change once), the call update has `sharing: true` and
   the share replaces the camera. `call_video().local` shows what's being
   sent. If they cancel, nothing happens; no event.
4. `Command::StopSharing` stops it (or closes the picker). If the desktop
   ends it (its sharing indicator, or the shared window closed), the call
   update goes back to `sharing: false` the same way. A failure also sends
   `Event::Error`.

The portal only sends a new frame when the screen changes, so a still
screen may show nothing for a moment after the share starts. Once a frame
has come, it's resent every second while nothing changes.

Quality is a setting: `Command::SetShareQuality(ShareQuality { max_height,
fps })`. `max_height` caps the height (keeping the shape; 0 sends the
screen's own size, up to 4096x2160) and `fps` is 1 to 60. It's saved, comes
back in `Event::CallDevices` as `share_quality`, and applies to a share in
progress at once. `ShareQuality::PRESETS` has named choices for a menu;
the default is Desktop's 1080p at 15 fps. WebRTC still lowers the bitrate
on a poor connection.

## 10. Stopping

Call `backend.stop(Duration::from_secs(5))` from a non-async thread before
the process exits. It hangs up a call in progress (waiting up to two
seconds for the hangup to be sent, so the other side sees the call end),
finishes the work in hand and closes the databases cleanly. Stop on
SIGTERM, SIGINT and SIGHUP too: a process killed without stopping leaves
the other side in the call until it times out.

## Demo mode

`demo: true` seeds made-up chats, people, groups, stickers and media, and
answers sends locally. Nothing touches the network except link previews you
type and, with a Giphy key, GIF search. Use it to build and screenshot your
interface without an account.

Always give demo mode its own scratch `data_dir`. It writes into whatever
directory it gets, a real account's included.
