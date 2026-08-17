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
//! [`spawn_input_thread`]: aphid_code::tui::event::spawn_input_thread

pub mod app;
pub mod chats;
pub mod log;
mod render;

use std::io::Stdout;

use aphid_code::tui::event::{UiEvent, spawn_input_thread};
use aphid_code::tui::input::{Action, Input};
use aphid_nostr::nostr::key::Keys;
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc::UnboundedReceiver;

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

    let (events, mut keys) = aphid_code::tui::runtime::channel();
    spawn_input_thread(&events);

    let mut input = Input::default();
    input.set_prompt(false);

    let (mut terminal, ()) = setup()?;
    let result = drive(&mut terminal, &mut app, &mut input, &client, &mut keys).await;
    restore(&mut terminal)?;
    result
}

async fn drive(
    terminal: &mut Screen,
    app: &mut App,
    input: &mut Input,
    client: &Client,
    keys: &mut UnboundedReceiver<UiEvent>,
) -> std::io::Result<()> {
    terminal.draw(|frame| render::draw(frame, app, input))?;

    // Whether the colony has hung up. A closed connection answers `None` at
    // once and for ever, so the arm has to be turned off rather than left to
    // spin. The terminal stays open on what it already has: the person reads
    // the last of it and leaves when they are ready, instead of the window
    // going out at the moment it has something to say.
    let mut gone = false;

    loop {
        let mut sending = Vec::new();

        tokio::select! {
            message = client.recv(), if !gone => match message {
                Some(message) => sending = app.apply(&message),
                None => {
                    gone = true;
                    app.note("── the colony stopped ──");
                }
            },
            event = keys.recv() => match event {
                None => break,
                Some(UiEvent::Key(key)) => sending = key_pressed(app, input, key),
                Some(UiEvent::Paste(text)) => input.paste(&text),
                Some(_) => {}
            },
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
        // A colony has no shell, so a `!` line goes into the chat as a
        // message, exactly as it did before `!` meant something to the coding
        // agent's terminal.
        Action::Bang(command) => app.typed(&format!("!{command}")),
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
    // So a pasted block lands in the input box whole, rather than as one
    // submitted line per newline. Not every console has the mode; the ones
    // that refuse it behave as they always did.
    let _ = stdout.execute(EnableBracketedPaste);
    Ok((ratatui::Terminal::new(CrosstermBackend::new(stdout))?, ()))
}

fn restore(terminal: &mut Screen) -> std::io::Result<()> {
    let _ = terminal.backend_mut().execute(DisableBracketedPaste);
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    Ok(())
}
