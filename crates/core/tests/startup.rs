//! Starting with an empty data directory must ask for linking without touching the network.

use futures::{StreamExt, executor::block_on};
use langis_core::{Backend, Config, Event};

#[test]
fn fresh_install_needs_linking() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_owned(),
        db_key: [3u8; 32],
        device_name: "test".into(),
        demo: false,
        former_data_dirs: Vec::new(),
        call_lines: Default::default(),
    };
    let (backend, mut events) = Backend::start(config).unwrap();
    assert_eq!(block_on(events.next()), Some(Event::NeedsLinking));

    // AppDb queries run on the backend runtime and can be awaited from any executor.
    let db = backend.db().clone();
    let conversations = block_on(backend.spawn(async move { db.conversations().await })).unwrap();
    assert!(conversations.is_empty());

    assert!(dir.path().join("signal.db").exists());
    assert!(dir.path().join("app.db").exists());
    assert!(dir.path().join("attachments").is_dir());
    // Decrypted files live here: only the owner may enter.
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    // Both databases are SQLCipher-encrypted (no plaintext SQLite header).
    for file in ["signal.db", "app.db"] {
        let bytes = std::fs::read(dir.path().join(file)).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"), "{file} is not encrypted");
    }
}

#[test]
fn a_data_directory_is_used_by_one_backend_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let config = || Config {
        data_dir: dir.path().to_owned(),
        db_key: [4u8; 32],
        device_name: "test".into(),
        demo: true,
        former_data_dirs: Vec::new(),
        call_lines: Default::default(),
    };
    let (first, _events) = Backend::start(config()).unwrap();
    let second = Backend::start(config());
    assert!(second.err().unwrap().to_string().contains("already in use"));
    // Once the first has stopped, the directory is free again.
    first.stop(std::time::Duration::from_secs(5));
    let (again, _events) = Backend::start(config()).unwrap();
    again.stop(std::time::Duration::from_secs(5));
}
