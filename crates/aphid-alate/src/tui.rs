//! A terminal attached to a running alate.
//!
//! This is [`aphid_code::tui`]'s app with the agent taken out. The transcript,
//! the input box, the status line and the confirmation modal are the same
//! types, drawn the same way; what has changed is where their content comes
//! from. Instead of a channel fed by plugins in this process, it is a socket
//! fed by a daemon in another.
//!
//! An alate holds several conversations at once, so this holds a [`Scrollback`] for
//! each one it has looked at and draws whichever is current. Switching is
//! [`Request::Watch`]: the daemon replays that session from its transcript, so
//! one that finished last week draws exactly like one running now.
//!
//! Nothing here owns a conversation. The session opened for this terminal ends
//! when the terminal does; the resident session and anything a job ran do not.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use aphid_code::plugins::permissions::Decision;
use aphid_code::tui::event::{UiEvent, spawn_input_thread};
use aphid_code::tui::input::{Action, Input};
use aphid_code::tui::modal::{Confirm, Modal};
use aphid_code::tui::scrollback::Scrollback;
use aphid_code::tui::status::Status;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{ExecutableCommand, cursor};
use ratatui::layout::{Constraint, Layout};
use ratatui::prelude::CrosstermBackend;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use std::io::Stdout;
use std::time::Duration;

use crate::gateway::Client;
use crate::gateway::wire::{Answer, Envelope, Frame as Wire, Request};
use crate::home::Home;

/// How often the screen is repainted while something is streaming.
const FRAME: Duration = Duration::from_millis(33);

/// The input box grows to this many rows, then scrolls.
const MAX_INPUT_ROWS: u16 = 4;

/// How many transcript lines PageUp and PageDown move.
const PAGE_LINES: usize = 10;

const HELP: &str = "\
/sessions      what conversations there are, running and stored
/session <id>  look at one of them
/new           start another conversation here
/log           show or hide notices, heartbeats and jobs
/clear         clear this screen, not the alate's memory
/help          this
/quit          detach, and leave the alate running

Esc cancels the run in this session. Ctrl-C detaches.
Everything else goes to the session you are looking at.";

type Screen = Terminal<CrosstermBackend<Stdout>>;

/// Everything the attached terminal holds.
struct App {
    /// One for each session looked at. Switching draws a different one rather
    /// than fetching the same history twice.
    views: HashMap<String, Scrollback>,
    /// The session being drawn and typed into.
    current: String,
    /// The session being filled from a replay, if one is arriving.
    filling: Option<String>,
    input: Input,
    status: Status,
    instance: String,
    modal: Option<Modal>,
    /// The confirmation on screen, and the channel the modal answers into. The
    /// modal type wants a sender, so it gets a local one and the answer is
    /// carried to the daemon from the other end.
    asked: Option<(u64, Receiver<Decision>)>,
    /// Whether notices, heartbeats and jobs are drawn. On by default: watching
    /// an alate work between prompts is most of the reason to attach.
    show_log: bool,
    quit: bool,
}

impl App {
    /// The view for the session on screen, made if this is the first frame for
    /// it.
    fn scrollback(&mut self) -> &mut Scrollback {
        self.views.entry(self.current.clone()).or_default()
    }

    fn view_of(&mut self, id: &str) -> &mut Scrollback {
        self.views.entry(id.to_owned()).or_default()
    }
}

/// Attach to an instance and draw it until the user detaches.
///
/// # Errors
///
/// Fails when there is no terminal, when nothing is listening on the socket, or
/// when the terminal cannot be set up.
pub async fn run(home: &Home) -> std::io::Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "attaching needs a terminal",
        ));
    }

    // One connection, not a probe and then a connection: connecting *is* the
    // check, and a second one would open a conversation nobody asked for.
    let socket = home.socket();
    let mut client = Client::connect(&socket).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!(
                "could not reach {}: {error}. Start it with `aphid alate run --name {}`",
                home.name(),
                home.name()
            ),
        )
    })?;
    let (events, mut keys) = aphid_code::tui::runtime::channel();
    spawn_input_thread(&events);

    let mut app = App {
        views: HashMap::new(),
        current: String::new(),
        filling: None,
        input: Input::default(),
        status: Status::default(),
        instance: home.name().to_owned(),
        modal: None,
        asked: None,
        show_log: true,
        quit: false,
    };

    let (mut terminal, _) = setup()?;
    let result = drive(&mut terminal, &mut app, &mut client, &mut keys).await;
    restore(&mut terminal)?;
    result
}

async fn drive(
    terminal: &mut Screen,
    app: &mut App,
    client: &mut Client,
    keys: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
) -> std::io::Result<()> {
    let mut dirty = true;

    while !app.quit {
        if std::mem::take(&mut dirty) {
            terminal.draw(|frame| render(frame, app))?;
        }

        tokio::select! {
            envelope = client.recv() => match envelope {
                Ok(Some(envelope)) => dirty = app.apply(envelope),
                // The daemon stopped, which is worth saying rather than
                // vanishing from under the user.
                Ok(None) => {
                    app.scrollback().push_notice("── the alate stopped ──");
                    app.status.running = false;
                    dirty = true;
                }
                Err(error) => {
                    app.scrollback().push_notice(format!("── lost the connection: {error} ──"));
                    dirty = true;
                }
            },
            key = keys.recv() => match key {
                None => break,
                Some(UiEvent::Key(key)) => {
                    dirty = true;
                    handle_key(app, key, client).await?;
                }
                Some(UiEvent::Paste(text)) => {
                    dirty = true;
                    // A modal takes single keys for an answer; a paste is not one.
                    if app.modal.is_none() {
                        app.input.paste(&text);
                    }
                }
                Some(UiEvent::Resize) => dirty = true,
                Some(_) => {}
            },
            // A repaint tick, so a streaming reply animates rather than
            // redrawing once for each token.
            () = tokio::time::sleep(FRAME), if app.status.running => dirty = true,
        }
    }
    Ok(())
}

impl App {
    /// Apply one envelope. `true` when the screen has to be drawn again.
    fn apply(&mut self, envelope: Envelope) -> bool {
        // A frame for a conversation this terminal is not looking at still
        // matters — it may be the session list changing — but it must not draw
        // over the one that is on screen.
        let session = envelope.session.clone();
        let mine = session.as_deref() == Some(self.current.as_str());

        match envelope.frame {
            Wire::Hello {
                instance,
                model,
                context_window,
                thinking,
            } => {
                self.instance = instance;
                self.status.model = model;
                self.status.context_window = context_window;
                self.status.thinking = thinking;
                // The envelope names the session opened for this terminal.
                if let Some(session) = session {
                    self.current = session;
                }
                let greeting = format!(
                    "── {} ──\nA conversation of your own. /sessions shows the others.",
                    self.instance
                );
                self.scrollback().push_notice(greeting);
            }
            Wire::HistoryStart { id } => {
                // Whatever was drawn for this session is stale; the replay that
                // follows is the whole of it.
                self.view_of(&id).clear();
                self.filling = Some(id);
            }
            Wire::HistoryEnd { .. } => self.filling = None,
            Wire::Sessions { live, stored } => {
                let mut text = String::from("sessions");
                for info in live.iter().chain(stored.iter()) {
                    let mark = if info.id == self.current { "*" } else { " " };
                    let running = if info.running { " running" } else { "" };
                    text.push_str(&format!(
                        "\n{mark} {}  {}  {}{running}",
                        info.id, info.kind, info.started
                    ));
                }
                text.push_str("\n\n/session <id> looks at one. An id can be shortened.");
                self.scrollback().push_notice(text);
            }
            Wire::SessionOpened { info } => {
                if !self.show_log || info.id == self.current {
                    return info.id == self.current;
                }
                self.scrollback()
                    .push_notice(format!("── {} started: {} ──", info.kind, info.id));
            }
            // The alate's own, so it is drawn wherever the terminal happens to
            // be looking.
            Wire::Heartbeat { at, note } => {
                if !self.show_log {
                    return false;
                }
                self.scrollback()
                    .push_notice(format!("── woke at {at} ──\n{note}"));
            }
            Wire::SessionClosed { id } => {
                if id == self.current {
                    self.scrollback().push_notice("── this session ended ──");
                } else {
                    self.views.remove(&id);
                    return false;
                }
            }
            // A permission question is the daemon's, not a conversation's: it
            // goes to every terminal, so whoever is at a keyboard can answer it
            // without first finding the session that asked.
            Wire::Confirm {
                id,
                tool,
                summary,
                risk,
            } => {
                let (reply, answer) = std::sync::mpsc::channel();
                self.asked = Some((id, answer));
                self.modal = Some(Modal::Confirm(Confirm {
                    tool,
                    summary,
                    risk: risk.into(),
                    reply,
                }));
            }
            // Everything below is a conversation's, so it is drawn into that
            // conversation's view whether or not it is the one on screen.
            frame => {
                let Some(id) = session else {
                    return false;
                };
                let show_log = self.show_log;
                let view = self.view_of(&id);
                match frame {
                    Wire::TurnStarted => {
                        // Block indices restart with each turn's message buffer.
                        view.clear_tool_streams();
                        if mine {
                            self.status.running = true;
                        }
                    }
                    Wire::Text { text } => view.push_text(&text),
                    Wire::Thinking { text } => view.push_thinking(&text),
                    Wire::ToolStreamStart { block, name } => view.begin_tool_stream(block, &name),
                    Wire::ToolStreamDelta { block, bytes } => view.push_tool_stream(block, bytes),
                    Wire::ToolCall {
                        id,
                        name,
                        arguments,
                    } => view.push_tool_call(&id, &name, &arguments),
                    Wire::ToolProgress { id, chunk } => view.push_tool_progress(&id, &chunk),
                    Wire::ToolResult {
                        id,
                        text,
                        is_error,
                        details,
                        ..
                    } => view.finish_tool(&id, &text, is_error, details),
                    Wire::TurnEnded { usage, error, .. } => {
                        // Runs after every call and result for the turn, so
                        // anything still streaming is a call that never arrived.
                        view.settle_tool_streams();
                        if let Some(error) = error {
                            view.push_notice(format!("error: {error}"));
                        }
                        if mine {
                            self.status.last = Some(usage);
                            self.status.total += usage;
                        }
                    }
                    Wire::RunEnded { .. } => {
                        if mine {
                            self.status.running = false;
                        }
                    }
                    Wire::Prompt { text } => view.push_user(text),
                    Wire::Notice { text } => {
                        if !show_log {
                            return false;
                        }
                        view.push_notice(text);
                    }
                    _ => {}
                }
                return mine;
            }
        }
        true
    }
}

async fn handle_key(app: &mut App, key: KeyEvent, client: &mut Client) -> std::io::Result<()> {
    if app.modal.is_some() {
        let decision = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Decision::Allow),
            KeyCode::Char('a' | 'A') => Some(Decision::AllowAlways),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision
            && let Some(Modal::Confirm(confirm)) = app.modal.take()
        {
            // Through the modal's own channel and out the other end, so the
            // modal keeps its type and this keeps its socket.
            confirm.answer(decision);
            if let Some((id, answer)) = app.asked.take() {
                let decision = answer.try_recv().unwrap_or(Decision::Deny);
                client
                    .send(&Request::Answer {
                        id,
                        decision: answer_of(decision),
                    })
                    .await?;
            }
        }
        return Ok(());
    }

    match app.input.handle(key) {
        Action::None => {}
        Action::Quit => app.quit = true,
        Action::Cancel => {
            if app.status.running {
                client.send(&Request::Cancel).await?;
                app.scrollback().push_notice("── cancelled ──");
            } else {
                app.input.clear();
            }
        }
        Action::ScrollUp => app.scrollback().scroll_up(PAGE_LINES),
        Action::ScrollDown => app.scrollback().scroll_down(PAGE_LINES),
        Action::ToggleThinking => {
            let view = app.scrollback();
            view.show_thinking = !view.show_thinking;
        }
        // There is no model picker here: the model belongs to the alate, and
        // terminals on different sessions must not be able to disagree about it.
        Action::CycleModel => app
            .scrollback()
            .push_notice("the model is the alate's; set it in alate.json"),
        Action::Submit(line) => {
            if let Some(rest) = line.strip_prefix('/') {
                return command(app, rest.trim(), client).await;
            }
            // Not echoed here: the daemon sends the prompt back to everybody
            // watching, so echoing would show it twice in this terminal.
            client.send(&Request::Prompt { text: line }).await?;
        }
        // The alate terminal has no shell of its own, so a `!` line goes to
        // the agent as a message, exactly as it did before `!` meant something
        // to the coding agent's terminal.
        Action::Bang(command) => {
            client
                .send(&Request::Prompt {
                    text: format!("!{command}"),
                })
                .await?;
        }
    }
    Ok(())
}

/// The commands a terminal answers, or turns into a request.
///
/// Anything else goes to the agent, including a line that opens with a slash: a
/// plugin in the daemon may have registered it, and this side does not know
/// what it has.
async fn command(app: &mut App, line: &str, client: &mut Client) -> std::io::Result<()> {
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    match name {
        "quit" | "exit" | "detach" => app.quit = true,
        "clear" => app.scrollback().clear(),
        "help" => app.scrollback().push_notice(HELP),
        "sessions" => client.send(&Request::Sessions).await?,
        "new" => client.send(&Request::New).await?,
        "session" => {
            let id = rest.trim();
            if id.is_empty() {
                app.scrollback()
                    .push_notice("which one? /sessions lists them");
            } else {
                // The daemon resolves a shortened id, because it is the one
                // that knows every session there has ever been.
                client.send(&Request::Watch { id: id.to_owned() }).await?;
                app.current = id.to_owned();
            }
        }
        "log" => {
            app.show_log = !app.show_log;
            let state = if app.show_log { "shown" } else { "hidden" };
            app.scrollback()
                .push_notice(format!("notices, heartbeats and jobs are {state}"));
        }
        _ => {
            let text = format!("no command /{name}; try /help");
            app.scrollback().push_notice(text);
        }
    }
    Ok(())
}

fn answer_of(decision: Decision) -> Answer {
    match decision {
        Decision::Allow => Answer::Allow,
        Decision::AllowAlways => Answer::AllowAlways,
        Decision::Deny => Answer::Deny,
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let content_height = (app.input.line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    // +2 for the border's top and bottom rows.
    let [transcript, input_row, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(content_height + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let view = app.views.entry(app.current.clone()).or_default();
    let visible = view.visible_lines(transcript.width as usize, transcript.height as usize);
    frame.render_widget(Paragraph::new(visible), transcript);

    app.input.set_prompt(app.status.running);
    frame.render_widget(app.input.textarea(), input_row);
    app.input.sync_scroll(content_height as usize);

    frame.render_widget(Paragraph::new(app.status.line()), status);

    if let Some(modal) = &app.modal {
        modal.render(frame, frame.area());
    }
}

fn setup() -> std::io::Result<(Screen, bool)> {
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
    Ok((Terminal::new(CrosstermBackend::new(stdout))?, false))
}

fn restore(terminal: &mut Screen) -> std::io::Result<()> {
    let _ = terminal.backend_mut().execute(DisableBracketedPaste);
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.backend_mut().execute(cursor::Show)?;
    Ok(())
}
