//! The smallest Langis frontend: start the backend on demo data, list the
//! chats, send a message, and print what comes back.
//!
//! Run it with `cargo run -p langis-core --example demo_frontend`. It never
//! connects to Signal: demo mode seeds made-up chats in a scratch directory
//! and echoes sends locally. A real frontend runs the same steps with
//! `demo: false` and its own data directory and key (see docs/frontend-guide.md).

use std::time::Duration;

use futures::{StreamExt as _, executor::block_on};
use langis_core::{Backend, Command, Config, Event};

fn main() -> anyhow::Result<()> {
    // 1. Start the backend. It runs on its own threads; this thread only
    //    sends commands and reads events.
    let dir = tempfile::tempdir()?;
    let config = Config {
        data_dir: dir.path().to_owned(),
        // A real frontend gets this from `langis_core::keys::load_or_create`.
        db_key: [7; 32],
        device_name: "Langis demo".into(),
        demo: true,
        former_data_dirs: Vec::new(),
        call_lines: Default::default(),
    };
    let (backend, mut events) = Backend::start(config)?;

    // 2. Wait for the account to be ready. A real account may first send
    //    NeedsLinking and LinkUrl (show that URL as a QR code to scan).
    loop {
        match block_on(events.next()) {
            Some(Event::Ready { aci, .. }) => {
                println!("ready as {aci}");
                break;
            }
            Some(Event::NeedsLinking) => anyhow::bail!("demo mode never needs linking"),
            Some(other) => println!("event: {other:?}"),
            None => anyhow::bail!("backend stopped"),
        }
    }

    // 3. Read state from the store. Queries run on the backend's runtime;
    //    `Backend::spawn` returns a future any executor can wait on.
    let db = backend.db().clone();
    let chats = block_on(backend.spawn(async move { db.conversations().await }))?;
    println!("\n{} chats:", chats.len());
    for chat in &chats {
        let preview = chat.last_message_preview.as_deref().unwrap_or("");
        println!("  {:<24} {:>3} unread  {preview}", chat.title, chat.unread_count);
    }

    // 4. Send a command. Results come back as events.
    let chat = chats.first().ok_or_else(|| anyhow::anyhow!("no chats"))?.id.clone();
    backend.send(Command::SendText {
        conversation: chat.clone(),
        body: "Hello from Langis".into(),
        quote: None,
        preview: None,
        mentions: Vec::new(),
        styles: Vec::new(),
    });

    // 5. Handle events until our message shows up.
    let sent = loop {
        match block_on(events.next()) {
            Some(Event::MessageChanged { conversation, message_id }) if conversation == chat => break message_id,
            Some(Event::Error(error)) => anyhow::bail!("{error}"),
            Some(_) => {}
            None => anyhow::bail!("backend stopped"),
        }
    };
    let db = backend.db().clone();
    let message = block_on(backend.spawn(async move { db.message(sent).await }))?
        .ok_or_else(|| anyhow::anyhow!("message {sent} not found"))?;
    println!("\nsent {:?} to {}: {:?}", message.body, chats[0].title, message.status);

    // 6. Stop cleanly so the databases are closed before exit.
    backend.stop(Duration::from_secs(5));
    Ok(())
}
