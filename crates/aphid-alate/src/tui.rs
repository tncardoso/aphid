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

use aphid_code::plugins::permissions::Decision;
use aphid_code::tui::input::{Action, Input};
use aphid_code::tui::modal::{Confirm, Modal};
use aphid_code::tui::render::ScrollbackCache;
use aphid_code::tui::runtime::{
    self, Cmd, Draw, Effects, Hub, Program, Subs, Timer, restore, setup,
};
use aphid_code::tui::scrollback::{Scrollback, Viewport};
use aphid_code::tui::status::Status;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;
use std::time::Duration;

use crate::gateway::wire::{Answer, Envelope, Frame as Wire, Request};
use crate::gateway::{Client, Reader, Writer};
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
/ps            what the alate has running
/kill <id>     stop a process
/log           show or hide notices, heartbeats and jobs
/clear         clear this screen, not the alate's memory
/help          this
/quit          detach, and leave the alate running

Esc cancels the run in this session. Ctrl-C detaches.
Everything else goes to the session you are looking at.";

/// What the attached terminal reacts to.
#[derive(Clone, Debug)]
pub enum Msg {
    /// Something the daemon said.
    Wire(Box<Envelope>),
    /// The daemon closed the connection.
    Stopped,
    /// The connection broke.
    Lost(String),
    Key(KeyEvent),
    Paste(String),
    Resize,
    /// The repaint tick, while something is streaming.
    Frame,
    /// What the last draw of the current pane laid out.
    LaidOut(Viewport),
}

/// What the attached terminal wants done.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Ask the daemon for something.
    Send(Box<Request>),
    Quit,
}

/// Everything the attached terminal holds.
pub struct App {
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
    /// Whether notices, heartbeats and jobs are drawn. On by default: watching
    /// an alate work between prompts is most of the reason to attach.
    show_log: bool,
    quit: bool,
}

impl App {
    /// What one session's pane holds, for a test to read.
    #[must_use]
    pub fn pane(&self, id: &str) -> Option<&Scrollback> {
        self.views.get(id)
    }

    /// Whether the session on screen is streaming.
    #[must_use]
    pub fn running(&self) -> bool {
        self.status.running
    }

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
    let client = Client::connect(&socket).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!(
                "could not reach {}: {error}. Start it with `aphid alate run --name {}`",
                home.name(),
                home.name()
            ),
        )
    })?;

    let (hub, mut inbox) = runtime::channel();
    // This terminal has no panels and nothing to click, so a mouse event is
    // nothing it can answer.
    runtime::spawn_input_thread(hub.clone(), |event| match event {
        ratatui::crossterm::event::Event::Key(key) => Some(Msg::Key(key)),
        ratatui::crossterm::event::Event::Paste(text) => Some(Msg::Paste(text)),
        ratatui::crossterm::event::Event::Resize(_, _) => Some(Msg::Resize),
        _ => None,
    });
    let (reader, writer) = client.split();
    spawn_reader(reader, hub.clone());

    let mut app = App::new(home.name());
    let mut effects = Socket::spawn(writer);

    let (mut terminal, kitty) = setup()?;
    let result = runtime::run(&mut app, &mut effects, &mut terminal, &hub, &mut inbox).await;
    restore(&mut terminal, kitty)?;
    result
}

impl App {
    /// A terminal that has heard nothing yet.
    #[must_use]
    pub fn new(instance: &str) -> Self {
        Self {
            views: HashMap::new(),
            current: String::new(),
            filling: None,
            input: Input::default(),
            status: Status::default(),
            instance: instance.to_owned(),
            modal: None,
            show_log: true,
            quit: false,
        }
    }
}

/// Turn everything the daemon says into messages, until it stops saying it.
fn spawn_reader(mut reader: Reader, hub: Hub<Msg>) {
    tokio::spawn(async move {
        loop {
            let msg = match reader.recv().await {
                Ok(Some(envelope)) => Msg::Wire(Box::new(envelope)),
                Ok(None) => Msg::Stopped,
                Err(error) => Msg::Lost(error.to_string()),
            };
            let last = !matches!(msg, Msg::Wire(_));
            if !hub.send(msg) || last {
                return;
            }
        }
    });
}

/// The writing half of the connection, and the task that owns it.
///
/// A request is queued rather than awaited: writing to a socket is waiting,
/// and the loop that draws the screen does not wait for anything.
struct Socket {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
}

impl Socket {
    fn spawn(mut writer: Writer) -> Self {
        let (requests, mut inbox) = tokio::sync::mpsc::unbounded_channel::<Request>();
        tokio::spawn(async move {
            while let Some(request) = inbox.recv().await {
                if writer.send(&request).await.is_err() {
                    // The reader will notice too, and say so on the screen.
                    return;
                }
            }
        });
        Self { requests }
    }
}

impl Effects for Socket {
    type Program = App;

    fn perform(&mut self, effect: Effect, _hub: &Hub<Msg>) {
        match effect {
            Effect::Send(request) => {
                let _ = self.requests.send(*request);
            }
            // Dropping the sender ends the writing task; the daemon sees the
            // socket close and stops watching for this terminal.
            Effect::Quit => {}
        }
    }
}

impl Program for App {
    type Msg = Msg;
    type Effect = Effect;

    fn update(&mut self, msg: Msg) -> Cmd<Effect> {
        let cmd = match msg {
            Msg::Wire(envelope) => self.arrived(*envelope),
            Msg::Stopped => {
                self.scrollback().push_notice("── the alate stopped ──");
                self.status.running = false;
                Cmd::none()
            }
            Msg::Lost(error) => {
                self.scrollback()
                    .push_notice(format!("── lost the connection: {error} ──"));
                Cmd::none()
            }
            Msg::Key(key) => self.keyed(key),
            Msg::Paste(text) => {
                // A modal takes single keys for an answer; a paste is not one.
                if self.modal.is_none() {
                    self.input.paste(&text);
                }
                Cmd::none()
            }
            Msg::Resize | Msg::Frame => Cmd::none(),
            Msg::LaidOut(viewport) => {
                self.scrollback().laid_out(viewport);
                Cmd::none()
            }
        };
        self.input.set_prompt(self.status.running);
        self.input
            .sync_scroll(self.input.line_count().clamp(1, MAX_INPUT_ROWS as usize));
        cmd
    }

    fn timer(&self, timer: Timer) -> Option<Msg> {
        (timer == Timer::Frame).then_some(Msg::Frame)
    }

    fn subs(&self) -> Subs {
        Subs {
            frame: self.status.running.then_some(FRAME),
            ..Subs::default()
        }
    }

    fn done(&self) -> bool {
        self.quit
    }
}

/// What the last draw wrapped, for each session that has been looked at.
///
/// One for each so that switching back to a conversation does not re-wrap its
/// whole history.
#[derive(Default)]
pub struct Caches {
    panes: HashMap<String, ScrollbackCache>,
    laid_out: Option<Viewport>,
}

impl Draw for App {
    type Cache = Caches;

    fn draw(&self, frame: &mut Frame<'_>, cache: &mut Caches) {
        let content_height = (self.input.line_count() as u16).clamp(1, MAX_INPUT_ROWS);
        // +2 for the border's top and bottom rows.
        let [transcript, input_row, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(content_height + 2),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let pane = cache.panes.entry(self.current.clone()).or_default();
        if let Some(view) = self.views.get(&self.current) {
            let laid = pane.layout(view, transcript.width as usize, transcript.height as usize);
            cache.laid_out = Some(laid);
            frame.render_widget(Paragraph::new(pane.visible(laid)), transcript);
        }

        frame.render_widget(self.input.textarea(), input_row);
        frame.render_widget(Paragraph::new(self.status.line()), status);

        if let Some(modal) = &self.modal {
            modal.render(frame, frame.area());
        }
    }

    fn laid_out(cache: &Caches) -> Option<Msg> {
        cache.laid_out.map(Msg::LaidOut)
    }
}

impl App {
    /// Apply one envelope.
    ///
    /// Everything the daemon says lands here, whether or not it is about the
    /// conversation on screen: a frame for another session still changes what
    /// that session's pane will show when it is looked at.
    fn arrived(&mut self, envelope: Envelope) -> Cmd<Effect> {
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
            Wire::Processes { live } => {
                // The answer to this terminal's own `/ps`: a direct reply, so
                // it is this connection's alone and needs no `mine` guard.
                let mut text = String::from("processes");
                if live.is_empty() {
                    text.push_str("\nnothing running");
                }
                for process in live {
                    let pid = process
                        .pid
                        .map_or_else(|| "—".to_owned(), |pid| pid.to_string());
                    text.push_str(&format!(
                        "\n#{:<3} {:>7} {:<9} {:<9} {}",
                        process.id, pid, process.origin, process.status, process.command
                    ));
                }
                text.push_str("\n\n/kill <id> stops one");
                self.scrollback().push_notice(text);
            }
            Wire::SessionOpened { info } => {
                if !self.show_log || info.id == self.current {
                    return Cmd::none();
                }
                self.scrollback()
                    .push_notice(format!("── {} started: {} ──", info.kind, info.id));
            }
            // The alate's own, so it is drawn wherever the terminal happens to
            // be looking.
            Wire::Heartbeat { at, note } => {
                if !self.show_log {
                    return Cmd::none();
                }
                self.scrollback()
                    .push_notice(format!("── woke at {at} ──\n{note}"));
            }
            Wire::SessionClosed { id } => {
                if id == self.current {
                    self.scrollback().push_notice("── this session ended ──");
                } else {
                    self.views.remove(&id);
                    return Cmd::none();
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
                // The daemon's own id travels with the question and comes back
                // with the answer, so nothing local has to remember it.
                self.modal = Some(Modal::Confirm(Confirm {
                    id,
                    tool,
                    summary,
                    risk: risk.into(),
                }));
            }
            // Everything below is a conversation's, so it is drawn into that
            // conversation's view whether or not it is the one on screen.
            frame => {
                let Some(id) = session else {
                    return Cmd::none();
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
                            return Cmd::none();
                        }
                        view.push_notice(text);
                    }
                    _ => {}
                }
                return Cmd::none();
            }
        }
        Cmd::none()
    }

    /// Handle one keypress.
    fn keyed(&mut self, key: KeyEvent) -> Cmd<Effect> {
        if self.modal.is_some() {
            let decision = match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Decision::Allow),
                KeyCode::Char('a' | 'A') => Some(Decision::AllowAlways),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Decision::Deny),
                _ => None,
            };
            if let Some(decision) = decision
                && let Some(Modal::Confirm(confirm)) = self.modal.take()
            {
                return ask(Request::Answer {
                    id: confirm.id,
                    decision: answer_of(decision),
                });
            }
            return Cmd::none();
        }

        match self.input.handle(key) {
            Action::None => Cmd::none(),
            Action::Quit => {
                self.quit = true;
                Cmd::one(Effect::Quit)
            }
            Action::Cancel => {
                if self.status.running {
                    self.scrollback().push_notice("── cancelled ──");
                    ask(Request::Cancel)
                } else {
                    self.input.clear();
                    Cmd::none()
                }
            }
            Action::ScrollUp => {
                self.scrollback().scroll_up(PAGE_LINES);
                Cmd::none()
            }
            Action::ScrollDown => {
                self.scrollback().scroll_down(PAGE_LINES);
                Cmd::none()
            }
            Action::ToggleThinking => {
                let view = self.scrollback();
                view.show_thinking = !view.show_thinking;
                Cmd::none()
            }
            // There is no model picker here: the model belongs to the alate,
            // and terminals on different sessions must not be able to
            // disagree about it.
            Action::CycleModel => {
                self.scrollback()
                    .push_notice("the model is the alate's; set it in alate.json");
                Cmd::none()
            }
            Action::Submit(line) => match line.strip_prefix('/') {
                Some(rest) => {
                    let rest = rest.trim().to_owned();
                    self.command(&rest)
                }
                // Not echoed here: the daemon sends the prompt back to
                // everybody watching, so echoing would show it twice.
                None => ask(Request::Prompt { text: line }),
            },
            // The alate terminal has no shell of its own, so a `!` line goes
            // to the agent as a message, exactly as it did before `!` meant
            // something to the coding agent's terminal.
            Action::Bang(command) => ask(Request::Prompt {
                text: format!("!{command}"),
            }),
        }
    }

    /// The commands a terminal answers, or turns into a request.
    ///
    /// Anything else goes to the agent, including a line that opens with a
    /// slash: a plugin in the daemon may have registered it, and this side
    /// does not know what it has.
    fn command(&mut self, line: &str) -> Cmd<Effect> {
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "quit" | "exit" | "detach" => {
                self.quit = true;
                Cmd::one(Effect::Quit)
            }
            "clear" => {
                self.scrollback().clear();
                Cmd::none()
            }
            "help" => {
                self.scrollback().push_notice(HELP);
                Cmd::none()
            }
            "sessions" => ask(Request::Sessions),
            "new" => ask(Request::New),
            "ps" => ask(Request::Processes),
            "kill" => match rest.trim().parse::<u32>() {
                Ok(id) => ask(Request::Kill { id }),
                Err(_) => {
                    self.scrollback().push_notice("which one? /ps lists them");
                    Cmd::none()
                }
            },
            "session" => {
                let id = rest.trim();
                if id.is_empty() {
                    self.scrollback()
                        .push_notice("which one? /sessions lists them");
                    return Cmd::none();
                }
                // The daemon resolves a shortened id, because it is the one
                // that knows every session there has ever been.
                self.current = id.to_owned();
                self.views.entry(self.current.clone()).or_default();
                ask(Request::Watch { id: id.to_owned() })
            }
            "log" => {
                self.show_log = !self.show_log;
                let state = if self.show_log { "shown" } else { "hidden" };
                self.scrollback()
                    .push_notice(format!("notices, heartbeats and jobs are {state}"));
                Cmd::none()
            }
            _ => {
                let text = format!("no command /{name}; try /help");
                self.scrollback().push_notice(text);
                Cmd::none()
            }
        }
    }
}

/// One request for the daemon.
fn ask(request: Request) -> Cmd<Effect> {
    Cmd::one(Effect::Send(Box::new(request)))
}

fn answer_of(decision: Decision) -> Answer {
    match decision {
        Decision::Allow => Answer::Allow,
        Decision::AllowAlways => Answer::AllowAlways,
        Decision::Deny => Answer::Deny,
    }
}
