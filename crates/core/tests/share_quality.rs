//! The screen share quality is saved and comes back with the call devices
//! (on demo data, without the network).

use futures::{StreamExt, executor::block_on};
use langis_core::{Backend, Command, Config, Event, PORTAL_SOURCE, ShareQuality};

fn next_devices(events: &mut (impl StreamExt<Item = Event> + Unpin)) -> langis_core::CallDevices {
    loop {
        match block_on(events.next()) {
            Some(Event::CallDevices(devices)) => return devices,
            Some(_) => {}
            None => panic!("backend stopped"),
        }
    }
}

#[test]
fn share_quality_is_saved_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_owned(),
        db_key: [5u8; 32],
        device_name: "test".into(),
        demo: true,
        former_data_dirs: Vec::new(),
        call_lines: Default::default(),
    };
    let (backend, mut events) = Backend::start(config).unwrap();

    backend.send(Command::LoadCallDevices);
    assert_eq!(next_devices(&mut events).share_quality, ShareQuality::default());

    backend.send(Command::SetShareQuality(ShareQuality { max_height: 1440, fps: 30 }));
    assert_eq!(next_devices(&mut events).share_quality, ShareQuality::HIGH);

    // Out of range values are clamped before they're kept.
    backend.send(Command::SetShareQuality(ShareQuality { max_height: 99_999, fps: 1000 }));
    assert_eq!(next_devices(&mut events).share_quality, ShareQuality { max_height: 2160, fps: 60 });

    let db = backend.db().clone();
    let saved = block_on(backend.spawn(async move { db.setting(langis_core::SETTING_SHARE_QUALITY).await })).unwrap();
    assert_eq!(saved.as_deref(), Some("2160@60"));

    backend.send(Command::LoadShareSources);
    loop {
        match block_on(events.next()) {
            Some(Event::ShareSources(Ok(sources))) => {
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].id, PORTAL_SOURCE);
                break;
            }
            Some(_) => {}
            None => panic!("backend stopped"),
        }
    }
    backend.stop(std::time::Duration::from_secs(5));
}
