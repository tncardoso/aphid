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
//! [`Input`]: aphid_code::tui::input::Input
//! [`spawn_input_thread`]: aphid_code::tui::event::spawn_input_thread

pub mod app;
pub mod chats;
pub mod log;
mod render;

use std::io::Stdout;

use aphid_code::tui::event::{UiEvent, spawn_input_thread};
use aphid_code::tui::input::{Action, Input};
use aphid_nostr::nostr::key::Keys;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::client::Client;
use app::{App, Send};

type Screen = ratatui::Terminal<CrosstermBackend<Stdout>>;

/// How many rows a page moves the log by.
const PAGE: usize = 10;

/// What to open a terminal on.
pub struct Options {
    /// Where the colony is.
    pub url: String,
    /// The key this terminal talks with.
    pub keys: Keys,
    /// What to publish as a name, if the configuration named one.
    pub name: Option<String>,
    /// The relay, when this process is hosting it.
    ///
    /// `aphid colony run` is one process with both in it, so the relay ending
    /// has to end the terminal — and holding it here is also what keeps it
    /// alive, since dropping a [`Relay`] stops it.
    ///
    /// [`Relay`]: crate::relay::Relay
    #[cfg(feature = "relay")]
    pub relay: Option<crate::relay::Relay>,
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

    let client = Client::connect(&options.url).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!("could not reach {}: {error}", options.url),
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

    let (events, mut keys) = unbounded_channel();
    spawn_input_thread(events);

    let mut input = Input::default();
    input.set_prompt(false);

    #[cfg(feature = "relay")]
    let mut hosted = options.relay;

    let (mut terminal, ()) = setup()?;
    let result = drive(
        &mut terminal,
        &mut app,
        &mut input,
        &client,
        &mut keys,
        #[cfg(feature = "relay")]
        &mut hosted,
    )
    .await;
    restore(&mut terminal)?;
    result
}

async fn drive(
    terminal: &mut Screen,
    app: &mut App,
    input: &mut Input,
    client: &Client,
    keys: &mut UnboundedReceiver<UiEvent>,
    #[cfg(feature = "relay")] hosted: &mut Option<crate::relay::Relay>,
) -> std::io::Result<()> {
    terminal.draw(|frame| render::draw(frame, app, input))?;

    loop {
        let mut sending = Vec::new();

        tokio::select! {
            message = client.recv() => match message {
                Some(message) => sending = app.apply(&message),
                // Worth saying rather than vanishing from under the person.
                None => app.note("── the colony stopped ──"),
            },
            event = keys.recv() => match event {
                None => break,
                Some(UiEvent::Key(key)) => sending = key_pressed(app, input, key),
                Some(_) => {}
            },
            () = stopped(
                #[cfg(feature = "relay")] hosted,
            ) => break,
        }

        for message in sending {
            post(client, message).await;
        }
        if app.quit {
            break;
        }
        terminal.draw(|frame| render::draw(frame, app, input))?;
    }
    Ok(())
}

/// Wait for the hosted relay to stop, or for ever when there is none.
#[cfg(feature = "relay")]
async fn stopped(hosted: &mut Option<crate::relay::Relay>) {
    match hosted {
        Some(relay) => relay.joined().await,
        None => std::future::pending().await,
    }
}

#[cfg(not(feature = "relay"))]
async fn stopped() {
    std::future::pending().await
}

/// Send one thing, and say so in the log if it will not go.
async fn post(client: &Client, message: Send) {
    let _ = match message {
        Send::Publish(event) => client.publish(*event).await,
        Send::Subscribe(id, filters) => client.subscribe(&id, filters).await,
        Send::Unsubscribe(id) => client.unsubscribe(&id).await,
    };
}

/// One keypress.
///
/// Tab and BackTab move in the nav and are taken **before** the editor sees
/// them, so typing never moves the selection. Everything else is the editor's,
/// which is what makes this feel like the rest of aphid.
fn key_pressed(app: &mut App, input: &mut Input, key: KeyEvent) -> Vec<Send> {
    match key.code {
        KeyCode::Tab => {
            app.chats.step(1);
            return Vec::new();
        }
        KeyCode::BackTab => {
            app.chats.step(-1);
            return Vec::new();
        }
        // Shift and an arrow moves in the nav too, for anybody whose terminal
        // eats BackTab.
        KeyCode::Down | KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.chats
                .step(if key.code == KeyCode::Down { 1 } else { -1 });
            return Vec::new();
        }
        _ => {}
    }

    match input.handle(key) {
        Action::Submit(line) => app.typed(&line),
        Action::Quit => {
            app.quit = true;
            Vec::new()
        }
        Action::ScrollUp => {
            if let Some(id) = app.chats.selected().cloned() {
                app.logs.entry(id).or_default().scroll_up(PAGE);
            }
            // At the top of a log there may be more behind it.
            app.backfill().into_iter().collect()
        }
        Action::ScrollDown => {
            if let Some(id) = app.chats.selected().cloned() {
                app.logs.entry(id).or_default().scroll_down(PAGE);
            }
            Vec::new()
        }
        // Ctrl-T means "show the working" in the coding agent. The nearest
        // thing a chat has is the times.
        Action::ToggleThinking => {
            app.show_time = !app.show_time;
            Vec::new()
        }
        // Ctrl-P cycles models where there are models. A colony has none, and
        // saying so is better than doing nothing.
        Action::CycleModel => {
            app.note("a colony runs no model; Ctrl-T shows or hides the times");
            Vec::new()
        }
        Action::Cancel | Action::None => Vec::new(),
    }
}

fn setup() -> std::io::Result<(Screen, ())> {
    // Restore the terminal even when something panics, so a crash does not
    // leave the shell in raw mode.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        previous(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Ok((ratatui::Terminal::new(CrosstermBackend::new(stdout))?, ()))
}

fn restore(terminal: &mut Screen) -> std::io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    Ok(())
}
