//! Settings kept on this device: the screen share quality (which comes back
//! with the call devices) and the local settings (on demo data, offline).

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

#[test]
fn local_settings_are_saved() {
    use langis_core::{LocalSetting, Theme};
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_owned(),
        db_key: [6u8; 32],
        device_name: "test".into(),
        demo: true,
        former_data_dirs: Vec::new(),
        call_lines: Default::default(),
    };
    let (backend, mut events) = Backend::start(config).unwrap();
    backend.send(Command::SetLocalSetting(LocalSetting::Theme(Theme::Dark)));
    backend.send(Command::SetLocalSetting(LocalSetting::IncomingCalls(false)));
    backend.send(Command::SetLocalSetting(LocalSetting::AlwaysRelayCalls(true)));
    backend.send(Command::SetLocalSetting(LocalSetting::ImageOptimization(langis_core::media::Optimization::Fast)));
    backend.send(Command::SetLocalSetting(LocalSetting::GiphyKey("  abc123 ".into())));
    // Commands run in order: once the devices come back, the settings are in.
    backend.send(Command::LoadCallDevices);
    next_devices(&mut events);
    let db = backend.db().clone();
    let read = |key: &'static str| {
        let db = db.clone();
        block_on(backend.spawn(async move { db.setting(key).await })).unwrap()
    };
    assert_eq!(Theme::from_setting(read(langis_core::SETTING_THEME).as_deref()), Theme::Dark);
    assert_eq!(read(langis_core::SETTING_INCOMING_CALLS).as_deref(), Some("false"));
    assert_eq!(read(langis_core::SETTING_ALWAYS_RELAY_CALLS).as_deref(), Some("true"));
    assert_eq!(read(langis_core::SETTING_IMAGE_OPTIMIZATION).as_deref(), Some("fast"));
    assert_eq!(read(langis_core::SETTING_GIPHY_API_KEY).as_deref(), Some("abc123"));
    backend.stop(std::time::Duration::from_secs(5));
}
