use super::*;

/// A made-up recovery key (64 of the pool's characters).
const POOL: &str = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz01";

fn demo_keys() -> AccountKeys {
    let me = crate::demo::SELF.trim_start_matches("d:");
    AccountKeys {
        aci: Aci::parse_from_service_id_string(me).unwrap(),
        entropy_pool: POOL.into(),
        profile_key: [0x42; 32],
        media_root_key: Some([0x24; 32]),
    }
}

/// The demo's data in a scratch directory.
async fn demo_db(dir: &Path) -> AppDb {
    let db = AppDb::open_in_memory().await.unwrap();
    std::fs::create_dir_all(dir.join("attachments")).unwrap();
    crate::demo::seed(&db, dir).await.unwrap();
    crate::demo::seed_profiles(&db, dir).await.unwrap();
    db
}

#[test]
fn recovery_key_shows_as_desktop_shows_it() {
    assert_eq!(format_key("abcd0o12efgh", false), "ABCD 0O12 EFGH");
    assert_eq!(format_key("abcd0o12efgh", true), "ABCD =#12 EFGH");
    assert!(key_matches("ABCD 0O12 efgh", "abcd0o12efgh"));
    assert!(key_matches("abcd=#12efgh", "abcd0o12efgh"), "Desktop compares with ambiguous characters converted");
    assert!(!key_matches("abcd0o12efg", "abcd0o12efgh"));
    assert_eq!(key_hash("abc"), "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=", "base64 of SHA-256");
    assert_eq!(parse_last("1700000000000 /tmp/a b"), Some((1_700_000_000_000, PathBuf::from("/tmp/a b"))));
    assert!(is_snapshot_name("signal-backup-2026-11-20-10-05-59"));
    assert!(!is_snapshot_name("signal-backup-2026-11-20"));
}

#[test]
fn mentions_and_formatting_go_back_to_placeholders() {
    use langis_store::{MentionSpan, StyleSpan, TextStyle};
    // "hi @Maya Chen, ok" with the mention bold and "ok" italic.
    let body = "hi @Maya Chen, ok";
    let mentions = [MentionSpan { start: 3, len: 10, aci: "00000000-0000-4000-8000-000000000002".into() }];
    let styles = [StyleSpan { start: 3, len: 10, style: TextStyle::Bold }, StyleSpan { start: 15, len: 2, style: TextStyle::Italic }];
    let text = text_of(body, &mentions, &styles);
    assert_eq!(text.body, "hi \u{FFFC}, ok");
    let ranges: Vec<(u32, u32)> = text.bodyRanges.iter().map(|r| (r.start, r.length)).collect();
    assert_eq!(ranges, [(3, 1), (3, 1), (6, 2)]);
}

#[tokio::test]
async fn demo_backup_passes_libsignals_validator() {
    let dir = tempfile::tempdir().unwrap();
    let db = demo_db(dir.path()).await;
    let keys = demo_keys();
    let now = chrono::Utc::now().timestamp_millis();
    let frames = frames(&db, &keys, &[7; 32], now).await.unwrap();
    assert!(frames.stats.messages > 20, "{:?}", frames.stats);
    assert!(frames.stats.files > 0, "the demo's drawn photos are backed up: {:?}", frames.stats);
    let encrypted = encrypt(&frames.plain, &keys).unwrap();
    validate(&encrypted, &keys).unwrap();
    // The wrong key doesn't read it.
    let other = AccountKeys { entropy_pool: "z".repeat(64), ..keys.clone() };
    assert!(validate(&encrypted, &other).is_err());
}

#[tokio::test]
async fn export_writes_desktops_layout() {
    use std::sync::atomic::AtomicBool;
    let data = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let db = demo_db(data.path()).await;
    let keys = demo_keys();
    let base = choose_folder(&db, out.path()).await.unwrap();
    assert_eq!(base, out.path().join("SignalBackups"));
    let mut steps = Vec::new();
    let now = 1_763_600_000_000;
    let snapshot = export(&db, &keys, &base, now, |p| steps.push(p), &AtomicBool::new(false)).await.unwrap();
    assert!(snapshot.file_name().unwrap().to_string_lossy().starts_with("signal-backup-"));
    assert!(is_snapshot_name(&snapshot.file_name().unwrap().to_string_lossy()));
    assert_eq!(steps.first(), Some(&LocalBackupProgress::Messages));
    let Some(LocalBackupProgress::Attachments { done, total }) = steps.last() else { panic!("{steps:?}") };
    assert_eq!(done, total);
    verify_metadata(&std::fs::read(snapshot.join("metadata")).unwrap(), &keys).unwrap();
    validate(&std::fs::read(snapshot.join("main")).unwrap(), &keys).unwrap();
    let names = read_files_list(&std::fs::read(snapshot.join("files")).unwrap()).unwrap();
    assert!(!names.is_empty());
    for name in &names {
        assert!(base.join("files").join(&name[..2]).join(name).is_file(), "{name}");
    }

    // A file decrypts with the localKey its pointer gives, to the file
    // this device has; and a second backup copies nothing new.
    let secret = file_secret(&db).await.unwrap();
    let frames = frames(&db, &keys, &secret, now).await.unwrap();
    let job = &frames.files[0];
    let mut data = std::fs::read(base.join("files").join(&job.media_name[..2]).join(&job.media_name)).unwrap();
    presage::libsignal_service::attachment_cipher::decrypt_in_place(job.local_key, &mut data).unwrap();
    data.truncate(job.size as usize);
    assert_eq!(data, std::fs::read(&job.source).unwrap());

    let second = export(&db, &keys, &base, now + 1000, |_| {}, &AtomicBool::new(false)).await.unwrap();
    let third = export(&db, &keys, &base, now + 2000, |_| {}, &AtomicBool::new(false)).await.unwrap();
    // Two snapshots kept, the oldest pruned; their files stay.
    assert!(!snapshot.exists() && second.exists() && third.exists());
    for name in &names {
        assert!(base.join("files").join(&name[..2]).join(name).is_file());
    }

    // Cancelled: nothing of it is left.
    let cancelled = export(&db, &keys, &base, now + 3000, |_| {}, &AtomicBool::new(true)).await;
    assert_eq!(cancelled, Err(LocalBackupError::Cancelled));
    assert!(!base.join(format!("signal-backup-{}", folder_timestamp(now + 3000))).exists());

    // Turning backups off deletes the folder when asked.
    disable(&db, true).await.unwrap();
    assert!(!base.exists());
    assert_eq!(db.setting(SETTING_FOLDER).await.unwrap(), None);
}

/// What a chat's messages show, for comparing before and after.
fn shown(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        // Calls aren't imported by `history.rs` (Desktop keeps them as
        // call history, which langis doesn't import from a backup).
        .filter(|m| !matches!(m.notice, Some(Notice::Call { .. })))
        // Shared contacts aren't written (see `Stats::skipped`).
        .filter(|m| m.content.is_none())
        // View-once messages are written (without their file, as Desktop's
        // local backups) but `history.rs` doesn't import them.
        .filter(|m| m.view_once.is_none())
        .map(|m| match &m.notice {
            Some(notice) => format!("{} notice {notice:?}", m.sent_at),
            None => format!(
                "{} {:?} {:?} {} deleted={} files={} quote={:?} reactions={:?} poll={:?}",
                m.sent_at,
                m.direction,
                m.author,
                m.body,
                m.deleted,
                m.attachments.len(),
                m.quote.as_ref().map(|q| (q.sent_at, q.author.clone(), q.text.clone())),
                m.reactions.iter().map(|r| (r.emoji.clone(), r.author.clone())).collect::<Vec<_>>(),
                m.poll.as_ref().map(|p| (p.question.clone(), p.options.clone(), p.ended)),
            ),
        })
        .collect()
}

#[tokio::test]
async fn round_trip_through_the_history_import() {
    let dir = tempfile::tempdir().unwrap();
    let db = demo_db(dir.path()).await;
    let keys = demo_keys();
    let me = keys.aci.service_id_string();
    let now = chrono::Utc::now().timestamp_millis();
    let frames = frames(&db, &keys, &[7; 32], now).await.unwrap();
    let encrypted = encrypt(&frames.plain, &keys).unwrap();

    // Read it back as a transfer would be: the same encryption, with the
    // recovery key's backup key in place of the link's one-time key.
    let plain = crate::history::decrypt(encrypted, keys.backup_key().unwrap().0, keys.aci).unwrap();
    assert_eq!(plain, frames.plain);
    let fresh = AppDb::open_in_memory().await.unwrap();
    let imported = crate::history::import(&fresh, &plain, &me, |_| {}).await.unwrap();

    let mut before = db.conversations().await.unwrap();
    before.extend(db.archived_conversations().await.unwrap());
    let mut after = fresh.conversations().await.unwrap();
    after.extend(fresh.archived_conversations().await.unwrap());
    assert_eq!(imported.chats, frames.stats.chats);
    for chat in &before {
        let Some(back) = after.iter().find(|c| c.id == chat.id) else {
            panic!("{} ({}) is missing after the round trip", chat.title, chat.id.0);
        };
        assert_eq!((back.kind, back.archived, back.pinned), (chat.kind, chat.archived, chat.pinned), "{}", chat.title);
        if chat.kind == ConversationKind::Group {
            assert_eq!(back.title, chat.title);
            let mut members = chat.members.clone();
            members.retain(|m| *m != me);
            members.sort();
            let mut back_members = back.members.clone();
            back_members.retain(|m| *m != me);
            back_members.sort();
            assert_eq!(back_members, members, "{}", chat.title);
        }
        let mut old = Vec::new();
        let mut new = Vec::new();
        let mut after_id = None;
        while let Some(last) = {
            let page = db.messages_in_order(&chat.id, after_id, 200).await.unwrap();
            let last = page.last().map(|m| m.id);
            old.extend(page);
            last
        } {
            after_id = Some(last);
        }
        let mut after_id = None;
        while let Some(last) = {
            let page = fresh.messages_in_order(&chat.id, after_id, 200).await.unwrap();
            let last = page.last().map(|m| m.id);
            new.extend(page);
            last
        } {
            after_id = Some(last);
        }
        // What a backup leaves out on purpose: messages disappearing
        // within a day.
        old.retain(|m| m.expires_in == 0 || i64::from(m.expires_in) * 1000 > MINIMUM_LIFETIME_MS);
        assert_eq!(shown(&new), shown(&old), "{}", chat.title);
    }
}

/// Every notice the backup format has an update for, in a group and a
/// 1:1 chat: valid, and read back the same by the history import.
#[tokio::test]
async fn notices_pass_the_validator_and_come_back() {
    use langis_store::{GroupNotice as G, MessageRequestEvent};
    let dir = tempfile::tempdir().unwrap();
    let db = demo_db(dir.path()).await;
    let keys = demo_keys();
    let me = keys.aci.service_id_string();
    let maya = "00000000-0000-4000-8000-00000000a11c".to_owned();
    let jordan = "00000000-0000-4000-8000-0000000000b0".to_owned();
    let group = ConversationId("g:0000000000000000000000000000000000000000000000000000000000000001".into());
    let direct = ConversationId::direct(&maya);
    let m = || Some(maya.clone());
    let j = || Some(jordan.clone());
    let group_notices = [
        Notice::Group { by: m(), change: G::Title { title: "Hikers".into() } },
        Notice::Group { by: None, change: G::Description { description: Some("Trails".into()) } },
        Notice::Group { by: m(), change: G::Avatar },
        Notice::Group { by: j(), change: G::Added { member: j() } },
        Notice::Group { by: m(), change: G::Added { member: j() } },
        Notice::Group { by: j(), change: G::Removed { member: j() } },
        Notice::Group { by: m(), change: G::Removed { member: j() } },
        Notice::Group { by: m(), change: G::Admin { member: j(), admin: true } },
        Notice::Group { by: m(), change: G::InfoAccess { admins_only: true } },
        Notice::Group { by: m(), change: G::MemberAccess { admins_only: false } },
        Notice::Group { by: m(), change: G::LabelAccess { admins_only: true } },
        Notice::Group { by: m(), change: G::AnnouncementsOnly { on: true } },
        Notice::Group { by: j(), change: G::JoinedByLink { member: j() } },
        Notice::Group { by: j(), change: G::AcceptedInvite { member: j(), inviter: Inviter::Person { id: maya.clone() } } },
        Notice::Group { by: j(), change: G::Requested { member: j() } },
        Notice::Group { by: m(), change: G::Approved { member: j() } },
        Notice::Group { by: m(), change: G::Invited { member: None } },
        Notice::Group { by: m(), change: G::InvitedMany { count: 3 } },
        Notice::Group { by: m(), change: G::RequestRemoved { member: j() } },
        Notice::Group { by: m(), change: G::LinkOn { approval: true } },
        // Its requester's own withdrawal, not right after a request (the
        // store folds that pair into one notice).
        Notice::Group { by: j(), change: G::RequestRemoved { member: j() } },
        Notice::Group { by: m(), change: G::LinkApproval { on: false } },
        Notice::Group { by: m(), change: G::LinkReset },
        Notice::Group { by: m(), change: G::LinkOff },
        Notice::Group { by: Some(langis_store::UNKNOWN_ACTOR.into()), change: G::Avatar },
        Notice::Timer { by: m(), seconds: 3600 },
        Notice::SafetyNumberChanged { who: maya.clone() },
        Notice::Group { by: m(), change: G::Ended },
    ];
    let direct_notices = [
        Notice::Timer { by: None, seconds: 86_400 },
        Notice::Verified { who: maya.clone(), verified: true, local: false },
        Notice::Verified { who: maya.clone(), verified: false, local: false },
        Notice::NumberChanged { who: maya.clone() },
        Notice::SessionRefreshed,
        Notice::SessionEnded,
        Notice::DeliveryIssue { who: maya.clone() },
        Notice::JoinedSignal { who: maya.clone() },
        Notice::UnsupportedMessage { who: m() },
        Notice::MessageRequest { event: MessageRequestEvent::Accept, group: false, who: m() },
        Notice::Merged { who: maya.clone(), phone: Some("+15555550100".into()) },
        Notice::NumberIsTheirs { who: maya.clone(), phone: "+15555550100".into() },
        Notice::StartedWith { who: maya.clone(), previous: "+15555550100".into() },
        Notice::PollEnded { by: None, question: "Trail?".into(), sent_at: 1_000 },
        Notice::Pinned { by: None, author: m(), sent_at: 1_000 },
    ];
    let mut at = 1_700_000_000_000;
    for (chat, notices) in [(&group, &group_notices[..]), (&direct, &direct_notices[..])] {
        for notice in notices {
            at += 1000;
            db.insert_notice(chat, at, notice).await.unwrap();
        }
    }
    let frames = frames(&db, &keys, &[7; 32], at + 1000).await.unwrap();
    let encrypted = encrypt(&frames.plain, &keys).unwrap();
    validate(&encrypted, &keys).unwrap();
    let fresh = AppDb::open_in_memory().await.unwrap();
    crate::history::import(&fresh, &frames.plain, &me, |_| {}).await.unwrap();
    for (chat, notices) in [(&group, &group_notices[..]), (&direct, &direct_notices[..])] {
        let back: Vec<Notice> = fresh.messages(chat, None, 500).await.unwrap().into_iter().filter_map(|m| m.notice).collect();
        for notice in notices {
            assert!(back.contains(notice), "{notice:?} didn't come back in {}", chat.0);
        }
    }
}

/// An edited message: dated by its latest edit, its earlier versions as
/// revisions; the import reads both back.
#[tokio::test]
async fn edits_come_back_with_their_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = demo_db(dir.path()).await;
    let keys = demo_keys();
    let me = keys.aci.service_id_string();
    let chat = ConversationId::direct("00000000-0000-4000-8000-00000000a11c");
    let original = db.messages_in_order(&chat, None, 50).await.unwrap().into_iter().find(|m| m.direction == Direction::Outgoing && !m.body.is_empty() && m.notice.is_none()).unwrap();
    let edit_at = original.sent_at + 5_000;
    db.edit_message(original.id, "Yep! Thanks again", edit_at).await.unwrap();
    let history = [langis_store::EditRevision { sent_at: original.sent_at, body: original.body.clone(), mentions: Vec::new(), styles: Vec::new() }];
    db.set_edit_history(original.id, &history).await.unwrap();
    let frames = frames(&db, &keys, &[7; 32], chrono::Utc::now().timestamp_millis()).await.unwrap();
    validate(&encrypt(&frames.plain, &keys).unwrap(), &keys).unwrap();
    let fresh = AppDb::open_in_memory().await.unwrap();
    crate::history::import(&fresh, &frames.plain, &me, |_| {}).await.unwrap();
    let back = fresh.find_message(&chat, None, edit_at).await.unwrap().expect("found by its edit's timestamp");
    assert_eq!((back.body.as_str(), back.sent_at, back.edited), ("Yep! Thanks again", original.sent_at, true));
    let back_history = fresh.edit_history(back.id).await.unwrap();
    assert_eq!(back_history.iter().map(|r| (r.sent_at, r.body.as_str())).collect::<Vec<_>>(), [(original.sent_at, original.body.as_str())]);
}

#[test]
fn the_padded_gzip_holds_the_stream_and_hides_its_size() {
    use std::io::Read as _;
    // BackupInfo, then a recipient and a run of chat items, delimited.
    let mut plain = Vec::new();
    super::put(&mut plain, &proto::BackupInfo::new()).unwrap();
    let mut recipient = proto::Frame::new();
    recipient.item = Some(proto::frame::Item::Recipient(proto::Recipient::new()));
    super::put(&mut plain, &recipient).unwrap();
    for i in 0..2000 {
        let mut item = proto::ChatItem::new();
        item.dateSent = i;
        let mut frame = proto::Frame::new();
        frame.item = Some(proto::frame::Item::ChatItem(item));
        super::put(&mut plain, &frame).unwrap();
    }
    let gz = super::padded_gzip(&plain).unwrap();
    assert!(gz.len() as u64 >= libsignal_message_backup::padding::MINIMUM_BACKUP_BYTES, "padded to libsignal's floor");
    let mut back = Vec::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut back).unwrap();
    assert_eq!(back, plain, "the zeros after the gzip aren't part of it");
}
