# Working on Langis

Langis is a UI-free Signal client library in Rust, forked from Vernam
(knpwrs/vernam) without its macOS GPUI app. It links to a phone as a
secondary device, as Signal Desktop does. Frontends (egui, GTK, Qt, a TUI,
anything) sit on top of `langis-core`; none lives in this repository.

Crates: `langis-core` (networking, the `Backend` a frontend drives),
`langis-store` (the SQLCipher app database a frontend reads), `langis-calls`
(RingRTC), `langis-emoji` (emoji search data for pickers). `vendor/` holds
modified presage and libsignal-service-rs; `vendor/README.md` lists every
change and must be updated with any new one.

Linux is the target. Don't add platform-specific code for other systems.

## The frontend contract

- Frontends send `Command`s, receive `Event`s, and read rendered state from
  `langis_store::AppDb`. They never touch presage. Anything a frontend needs
  goes through one of those three, documented, so another frontend can use
  it too.
- No UI toolkit types in the core: no colors, widgets or layout. Return data
  (ids, text, paths, timestamps) and let the frontend draw it.
- A change to `Command`, `Event` or a public store type is a change to the
  public API: update `docs/` in the same commit.

## Signal Desktop is the reference

When behavior is in question (what a read sync marks, how a reaction finds its target, what a backup field means), read Signal Desktop's source and do what it does. Cite the file in a comment (`ts/util/safetyNumber`, `ts/messageModifiers/ReadSyncs`). `.signal-desktop-hash` names the last Desktop commit Langis has been checked against; the `desktop-parity` skill (`.claude/skills/desktop-parity`) catches up from there and moves it forward. Guessing from the protocol alone produced most of the real-account bugs:

- **Edited messages answer to every edit's timestamp.** Reactions, receipts and read syncs for an edited message name the latest edit, not the original. `message_edits` records each edit's timestamp and lookups match it. A transferred backup dates an edited message by its latest revision; the original date is the oldest revision's.
- **A read sync reads everything older in its chat**, and one that arrives before its message is kept and applied when the message does. Marking only the named message left chats unread.
- **Link and sync sends more than chats and messages.** The backup also lists installed sticker packs, and each attachment's transit-tier locator. Read every frame type before deciding one doesn't matter.

## presage caches things forever

presage stores the first profile, profile photo and group photo it downloads and never refreshes them. Anything that can change needs a fresh fetch: `profiles.rs` asks the server for the current avatar path, and `refresh_group_avatar` compares the group's avatar path. Name saved files after the remote path (hashed), so a changed path means a new download. Profile fetches are rate limited: check each person at most daily and cap each sync, as Desktop's routine profile refresh does.

## Database

- **Every store transaction starts with `BEGIN IMMEDIATE`** (`AppDb::write`). A deferred transaction that reads and then writes fails at once with "database is locked" when another connection committed in between; the busy timeout doesn't apply. This aborted a history import while messages were arriving.
- **Writes queue for one connection.** `AppDb` has a read pool and a writer pool of one connection; every write, a transaction or a single statement, goes through `writer`. Writes from two connections race for SQLite's lock, and the loser fails with "database is locked" after the 5-second busy timeout: a reaction, a receipt and an incoming message were lost that way. The read pool's connections are `query_only`, so a write sent to `pool` by mistake fails at once, and the store's tests run on disk with both pools to catch it.
- **Keep each write transaction short**, since everything else waits behind it: commit per item rather than holding the lock across a long loop, and make sure each statement in a loop uses an index (check with `EXPLAIN QUERY PLAN`). `rename_authors` once scanned every message twice per person, 1 to 3 seconds a scan, all in one transaction. `json_extract` lookups need an expression index.
- **Migrations are squashed into one init.** Changing the init changes its checksum, and sqlx refuses a database whose applied migration changed, so any change to the init needs a path in `adopt_squashed_history` for databases made by the previous init. Before adding a migration, check whether a JSON field with `#[serde(default)]` or a `settings` row will do: attachment locators and verified identity keys needed no migration.
- **The history import is not one transaction** and runs alongside the receive loop. Once it starts, nothing may cancel it, and a failure leaves a partial import that is marked done.

## Privacy and accounts

- Never use anyone's real messages, contacts or photos as test data or demo
  content. The demo's people are made up and its images are drawn in code.
- Tests and demo runs always use a scratch data directory. Never point a
  run at a real account's data directory.
- Don't ship Signal's Giphy key.

## Testing

- `Config::demo` runs the backend on seeded demo data with no network. Tests
  that read demo names or messages change together with `demo.rs`.
- `cargo test --workspace` must pass before every commit.

## Commits

- Don't run `cargo fmt`: the code isn't rustfmt-formatted, and it rewrites
  nearly every file in the crate.
- Atomic commits, one per change, each building and passing on its own.
- Findings that came up again and again in Vernam's reviews:
  - Don't consume queued state (an early read sync, a pending download) until the work it stands for succeeded.
  - Don't record side data for an update that didn't apply, such as an edit timestamp for an edit to a deleted message.
  - Don't decide what a file is by sniffing its bytes when the stored content type says. A file must never go through the image optimizer.
  - Bound network work per run and schedule the rest, rather than issuing hundreds of requests at once.
  - Report every reason something was skipped, not only the common one.

## Noise in the logs

- `could not create sync message from a group message` (libsignal-service) is logged whenever something other than a message or edit, such as a typing indicator, goes to a group. It's harmless.
- `trusting new identity` appears as sessions with contacts start after linking.

## Writing

Docs and comments: no em dashes, concrete details instead of claims, no
padding. Say what was tested and what wasn't.
