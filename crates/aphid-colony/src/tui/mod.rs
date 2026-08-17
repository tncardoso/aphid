//! A terminal on a colony.
//!
//! It reuses what [`aphid_code::tui`] already has and nothing it does not. The
//! editor is that crate's [`Input`], with its history and its multi-line
//! handling, and the keys arrive on [`spawn_input_thread`] because
//! `crossterm::event::read` blocks. What is new is everything about a chat: a
//! nav of groups, a log with authors in it, and no notion of a token, a cost or
//! a permission — a colony runs no agent and has none of those to show.
//!
//! There is no repaint timer either. Nothing here streams a word at a time, so
//! the screen is drawn when something changed and an idle colony costs nothing.
//!
//! Nothing here holds a relay. The terminal is a client of one that is already
//! running, in the same way the alate bridge is, so several terminals can watch
//! one colony and closing any of them leaves it alone.
//!
//! [`Input`]: aphid_code::tui::input::Input
//! [`spawn_input_thread`]: aphid_code::tui::runtime::spawn_input_thread

pub mod app;
pub mod chats;
pub mod log;
mod render;

use aphid_code::tui::runtime::{self, Draw, Effects, Hub, restore, setup};
use aphid_nostr::nostr::key::Keys;

use crate::client::Client;
use app::{App, Effect, Msg};

/// What to open a terminal on.
pub struct Options {
    /// Where the colony is.
    pub url: String,
    /// The key this terminal talks with.
    pub keys: Keys,
    /// What to publish as a name, if the configuration named one.
    pub name: Option<String>,
}

/// Open a terminal and run until it is closed.
///
/// # Errors
///
/// Fails when there is no terminal, or when the colony cannot be reached.
pub async fn run(options: Options) -> std::io::Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "a colony's terminal needs a terminal",
        ));
    }

    // The client's own error names the colony it could not reach, so this adds
    // the verb and not the address again.
    let client = Client::connect(&options.url).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!("could not reach {error}"),
        )
    })?;

    let mut app = App::new(options.keys, options.url.clone());
    for message in app
        .naming(options.name.as_deref())
        .into_iter()
        .chain(std::iter::once(app.opening()))
    {
        post(&client, message).await;
    }

    let client = std::sync::Arc::new(client);
    let (hub, mut inbox) = runtime::channel();
    // A colony has no panels and nothing to click, so a mouse event is nothing
    // it can answer.
    runtime::spawn_input_thread(hub.clone(), |event| match event {
        ratatui::crossterm::event::Event::Key(key) => Some(Msg::Key(key)),
        ratatui::crossterm::event::Event::Paste(text) => Some(Msg::Paste(text)),
        ratatui::crossterm::event::Event::Resize(_, _) => Some(Msg::Resize),
        _ => None,
    });
    spawn_reader(std::sync::Arc::clone(&client), hub.clone());

    let mut effects = Relay::spawn(client);
    let (mut terminal, kitty) = setup()?;
    let result = runtime::run(&mut app, &mut effects, &mut terminal, &hub, &mut inbox).await;
    restore(&mut terminal, kitty)?;
    result
}

/// Turn everything the colony says into messages, until it stops saying it.
fn spawn_reader(client: std::sync::Arc<Client>, hub: Hub<Msg>) {
    tokio::spawn(async move {
        loop {
            let Some(message) = client.recv().await else {
                // A closed connection answers `None` at once and for ever, so
                // this says so once and stops rather than spinning.
                hub.send(Msg::Gone);
                return;
            };
            if !hub.send(Msg::Relay(Box::new(message))) {
                return;
            }
        }
    });
}

/// The connection, and the task that writes to it.
///
/// Queued rather than awaited: publishing is waiting, and the loop that draws
/// the screen does not wait for anything.
struct Relay {
    sending: tokio::sync::mpsc::UnboundedSender<Effect>,
}

impl Relay {
    fn spawn(client: std::sync::Arc<Client>) -> Self {
        let (sending, mut inbox) = tokio::sync::mpsc::unbounded_channel::<Effect>();
        tokio::spawn(async move {
            while let Some(message) = inbox.recv().await {
                post(&client, message).await;
            }
        });
        Self { sending }
    }
}

impl Effects for Relay {
    type Program = App;

    fn perform(&mut self, effect: Effect, _hub: &Hub<Msg>) {
        let _ = self.sending.send(effect);
    }
}

impl Draw for App {
    // Nothing here is wrapped or cached: a chat log is short lines that are
    // already lines, so there is nothing a frame could usefully remember.
    type Cache = ();

    fn draw(&self, frame: &mut ratatui::Frame<'_>, (): &mut ()) {
        render::draw(frame, self, &self.input);
    }
}

/// Send one thing, and say so in the log if it will not go.
async fn post(client: &Client, message: Effect) {
    let _ = match message {
        Effect::Publish(event) => client.publish(*event).await,
        Effect::Subscribe(id, filters) => client.subscribe(&id, filters).await,
        Effect::Unsubscribe(id) => client.unsubscribe(&id).await,
    };
}
