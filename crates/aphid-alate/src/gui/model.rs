//! What the window holds, and how a frame changes it.
//!
//! The terminal's model in [`crate::tui`] is not reused, and the reason is not
//! taste: its `Scrollback`, `Status` and `Modal` are built out of `ratatui`
//! `Line`s and `Span`s. Those are terminal types, not neutral data, so sharing
//! them would put a terminal library behind a window. What is shared instead is
//! everything below the drawing — [`crate::gateway::Client`] and the wire — and
//! that is the part with a protocol to agree on.
//!
//! The taxonomy is drawn for a window. A tool call keeps its arguments as JSON
//! rather than as the text of them, because a window can lay a value out and a
//! terminal could only print it. Notices, heartbeats and session events are
//! three kinds and not one, so the log filter can one day be three switches.
//! There is no viewport, no scroll offset and no wrapped-line count: the window
//! measures its own lines.

use std::collections::HashMap;

use aphid_core::{Json, Usage};

use crate::gateway::wire::{Answer, Envelope, Frame, Request, Risk};
use crate::sessions::Info;

/// How far a tool call has got.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// Its arguments are still arriving, and the call has not been announced.
    Streaming,
    Running,
    Done,
    Failed,
}

/// One tool call, from the first byte of its arguments to its result.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    /// The arguments as a value when they parse, which is what lets the window
    /// lay them out instead of printing them.
    pub arguments: Option<Json>,
    /// The arguments as they arrived. Kept because a call that does not parse
    /// still has to be shown.
    pub raw: String,
    pub output: String,
    pub state: ToolState,
    /// What the tool said about itself: the diff of an edit, the rows of a
    /// query. The window draws it; the model only carries it.
    pub details: Option<Json>,
    /// Argument bytes counted while streaming, so a slow call still moves.
    pub streamed: usize,
}

/// One thing in a transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// A prompt, whoever sent it. The daemon echoes it back, so this is never
    /// written locally and never doubles.
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(Tool),
    /// A plugin, or an error, or anything else worth saying once.
    Notice(String),
    /// The alate woke on its own.
    Heartbeat {
        at: String,
        note: String,
    },
    /// A session started, ended, or was joined.
    Session(String),
}

/// One conversation, as this window has seen it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pane {
    entries: Vec<Entry>,
    /// The placeholder each streaming block is writing into.
    streams: HashMap<u32, usize>,
    /// Where each announced call landed.
    by_call: HashMap<String, usize>,
}

impl Pane {
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.streams.clear();
        self.by_call.clear();
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// Add to the last assistant entry, or start one.
    ///
    /// Text arrives in the pieces the provider sent it in, and a paragraph
    /// split across four frames is one message and not four.
    fn push_text(&mut self, text: &str) {
        match self.entries.last_mut() {
            Some(Entry::Assistant(body)) => body.push_str(text),
            _ => self.push(Entry::Assistant(text.to_owned())),
        }
    }

    fn push_thinking(&mut self, text: &str) {
        match self.entries.last_mut() {
            Some(Entry::Thinking(body)) => body.push_str(text),
            _ => self.push(Entry::Thinking(text.to_owned())),
        }
    }

    fn begin_stream(&mut self, block: u32, name: &str) {
        if self.streams.contains_key(&block) {
            return;
        }
        self.streams.insert(block, self.entries.len());
        self.push(Entry::Tool(Tool {
            name: name.to_owned(),
            arguments: None,
            raw: String::new(),
            output: String::new(),
            state: ToolState::Streaming,
            details: None,
            streamed: 0,
        }));
    }

    fn count_stream(&mut self, block: u32, bytes: usize) {
        let Some(&index) = self.streams.get(&block) else {
            return;
        };
        if let Some(Entry::Tool(tool)) = self.entries.get_mut(index) {
            tool.streamed += bytes;
        }
    }

    /// Forget which blocks were streaming. Block numbers restart with each
    /// turn's message buffer, so an old one would claim a new one's bytes.
    fn clear_streams(&mut self) {
        self.streams.clear();
    }

    /// Resolve placeholders whose call never arrived.
    ///
    /// A turn that failed or was cancelled mid-stream never announces its
    /// calls, and a card left counting would count for ever.
    fn settle_streams(&mut self) {
        let indices: Vec<usize> = self.streams.values().copied().collect();
        for index in indices {
            if let Some(Entry::Tool(tool)) = self.entries.get_mut(index)
                && tool.state == ToolState::Streaming
            {
                tool.state = ToolState::Failed;
                tool.output = "the turn ended before the call was complete".to_owned();
            }
        }
        self.streams.clear();
    }

    fn first_streaming(&self) -> Option<usize> {
        let mut indices: Vec<usize> = self.streams.values().copied().collect();
        indices.sort_unstable();
        indices.into_iter().find(|&index| {
            matches!(
                self.entries.get(index),
                Some(Entry::Tool(tool)) if tool.state == ToolState::Streaming
            )
        })
    }

    fn push_call(&mut self, id: &str, name: &str, arguments: &str) {
        let parsed = serde_json::from_str::<Json>(arguments).ok();
        // Calls are announced in the order their blocks streamed in, so the
        // first placeholder still waiting is this call's own.
        if let Some(index) = self.first_streaming()
            && let Some(Entry::Tool(tool)) = self.entries.get_mut(index)
        {
            tool.name = name.to_owned();
            tool.raw = arguments.to_owned();
            tool.arguments = parsed;
            tool.state = ToolState::Running;
            self.by_call.insert(id.to_owned(), index);
            return;
        }
        self.by_call.insert(id.to_owned(), self.entries.len());
        self.push(Entry::Tool(Tool {
            name: name.to_owned(),
            arguments: parsed,
            raw: arguments.to_owned(),
            output: String::new(),
            state: ToolState::Running,
            details: None,
            streamed: 0,
        }));
    }

    fn push_progress(&mut self, id: &str, chunk: &str) {
        let Some(&index) = self.by_call.get(id) else {
            return;
        };
        if let Some(Entry::Tool(tool)) = self.entries.get_mut(index) {
            tool.output.push_str(chunk);
            tool.output.push('\n');
        }
    }

    fn finish_call(&mut self, id: &str, text: &str, is_error: bool, details: Option<Json>) {
        let Some(&index) = self.by_call.get(id) else {
            return;
        };
        if let Some(Entry::Tool(tool)) = self.entries.get_mut(index) {
            // The result is authoritative; the progress chunks were a preview
            // of it, so they are replaced rather than added to.
            tool.output = text.to_owned();
            tool.state = if is_error {
                ToolState::Failed
            } else {
                ToolState::Done
            };
            tool.details = details;
        }
    }
}

/// What the status line says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub model: String,
    pub thinking: Option<String>,
    pub context_window: u32,
    pub total: Usage,
    pub last: Option<Usage>,
    /// Whether the session on screen has a run in flight.
    pub running: bool,
}

impl Status {
    /// The tokens the next request would carry.
    #[must_use]
    pub fn context_used(&self) -> u32 {
        self.last.map_or(0, |usage| {
            usage.input + usage.cache_read + usage.cache_write + usage.output
        })
    }
}

/// A tool waiting on an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub id: u64,
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
}

/// The sessions the daemon last reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sessions {
    pub live: Vec<Info>,
    pub stored: Vec<Info>,
}

/// Where the window stands with the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// No daemon answered. The window is open and the alate is asleep.
    Asleep,
    /// Trying, after the connection broke.
    Connecting,
    Connected,
}

/// Everything one window holds.
#[derive(Clone, Debug)]
pub struct Model {
    /// One for each conversation looked at, so switching back does not fetch
    /// the same history twice.
    panes: HashMap<String, Pane>,
    /// The conversation on screen and typed into.
    current: String,
    /// The conversation a replay is filling, if one is arriving.
    filling: Option<String>,
    pub status: Status,
    pub confirm: Option<Confirm>,
    pub sessions: Sessions,
    /// The alate this window is watching.
    pub instance: String,
    pub link: Link,
    /// Whether notices, heartbeats and session events are drawn. On by default:
    /// watching an alate work between prompts is most of the reason to have a
    /// window open at all.
    pub show_log: bool,
}

impl Model {
    /// A window that has heard nothing yet.
    #[must_use]
    pub fn new(instance: &str) -> Self {
        Self {
            panes: HashMap::new(),
            current: String::new(),
            filling: None,
            status: Status::default(),
            confirm: None,
            sessions: Sessions::default(),
            instance: instance.to_owned(),
            link: Link::Connecting,
            show_log: true,
        }
    }

    /// The conversation on screen.
    #[must_use]
    pub fn current(&self) -> &str {
        &self.current
    }

    /// The pane on screen, empty when nothing has arrived for it.
    #[must_use]
    pub fn pane(&self) -> &Pane {
        static EMPTY: std::sync::OnceLock<Pane> = std::sync::OnceLock::new();
        self.panes
            .get(&self.current)
            .unwrap_or_else(|| EMPTY.get_or_init(Pane::default))
    }

    /// One conversation's pane, for a test to read.
    #[must_use]
    pub fn pane_of(&self, id: &str) -> Option<&Pane> {
        self.panes.get(id)
    }

    /// Whether a replay is arriving.
    #[must_use]
    pub fn filling(&self) -> bool {
        self.filling.is_some()
    }

    fn view(&mut self) -> &mut Pane {
        self.panes.entry(self.current.clone()).or_default()
    }

    fn view_of(&mut self, id: &str) -> &mut Pane {
        self.panes.entry(id.to_owned()).or_default()
    }

    /// Everything drawn is thrown away and the panes start again.
    ///
    /// What a reconnection does: the daemon opens a session for each
    /// connection, so what comes back is a new conversation and not the old one
    /// carried on.
    pub fn reset(&mut self) {
        self.panes.clear();
        self.current.clear();
        self.filling = None;
        self.confirm = None;
        self.status.running = false;
    }

    /// Say this, whatever it is.
    ///
    /// A line beginning with `/` is a command for the window; anything else is
    /// a prompt for the agent. The prompt is not drawn here: the daemon echoes
    /// it back as [`Frame::Prompt`] to everybody watching, and writing it now
    /// would show it twice.
    #[must_use]
    pub fn submit(&mut self, line: &str) -> Vec<Request> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        if let Some(command) = line.strip_prefix('/') {
            return self.command(command);
        }
        vec![Request::Prompt {
            text: line.to_owned(),
        }]
    }

    /// Run one `/` command.
    #[must_use]
    pub fn command(&mut self, line: &str) -> Vec<Request> {
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "sessions" => vec![Request::Sessions],
            "new" => vec![Request::New],
            "session" => {
                let id = rest.trim();
                if id.is_empty() {
                    self.view()
                        .push(Entry::Notice("which one? /sessions lists them".to_owned()));
                    return Vec::new();
                }
                // The daemon resolves a shortened id: it is the one that knows
                // every session there has ever been.
                self.current = id.to_owned();
                self.panes.entry(self.current.clone()).or_default();
                vec![Request::Watch { id: id.to_owned() }]
            }
            "clear" => {
                self.view().clear();
                Vec::new()
            }
            "log" => {
                self.show_log = !self.show_log;
                let state = if self.show_log { "shown" } else { "hidden" };
                let text = format!("notices, heartbeats and jobs are {state}");
                self.view().push(Entry::Notice(text));
                Vec::new()
            }
            _ => {
                let text = format!("no command /{name}; try /sessions, /new, /clear or /log");
                self.view().push(Entry::Notice(text));
                Vec::new()
            }
        }
    }

    /// Watch this conversation instead.
    #[must_use]
    pub fn watch(&mut self, id: &str) -> Vec<Request> {
        self.current = id.to_owned();
        self.panes.entry(self.current.clone()).or_default();
        vec![Request::Watch { id: id.to_owned() }]
    }

    /// Answer the question on screen, if there is one.
    #[must_use]
    pub fn answer(&mut self, decision: Answer) -> Vec<Request> {
        let Some(confirm) = self.confirm.take() else {
            return Vec::new();
        };
        vec![Request::Answer {
            id: confirm.id,
            decision,
        }]
    }

    /// Say a line to the conversation on screen, from the window itself.
    pub fn note(&mut self, text: impl Into<String>) {
        self.view().push(Entry::Notice(text.into()));
    }

    /// Apply one envelope.
    ///
    /// Everything the daemon says lands here, whether or not it is about the
    /// conversation on screen: a frame for another one still changes what that
    /// pane will show when it is looked at.
    pub fn arrived(&mut self, envelope: Envelope) {
        let session = envelope.session.clone();
        let mine = session.as_deref() == Some(self.current.as_str());

        match envelope.frame {
            Frame::Hello {
                instance,
                model,
                context_window,
                thinking,
            } => {
                self.instance = instance;
                self.status.model = model;
                self.status.context_window = context_window;
                self.status.thinking = thinking;
                self.link = Link::Connected;
                // The envelope names the session opened for this window.
                if let Some(session) = session {
                    self.current = session;
                }
                self.panes.entry(self.current.clone()).or_default();
            }
            Frame::HistoryStart { id } => {
                // Whatever was drawn for this conversation is stale; what
                // follows is the whole of it.
                self.view_of(&id).clear();
                self.filling = Some(id);
            }
            Frame::HistoryEnd { .. } => self.filling = None,
            Frame::Sessions { live, stored } => self.sessions = Sessions { live, stored },
            Frame::SessionOpened { info } => {
                if self.show_log && info.id != self.current {
                    let text = format!("{} started: {}", info.kind, info.id);
                    self.view().push(Entry::Session(text));
                }
                if !self.sessions.live.iter().any(|live| live.id == info.id) {
                    self.sessions.live.push(info);
                }
            }
            Frame::SessionClosed { id } => {
                self.sessions.live.retain(|live| live.id != id);
                if id == self.current {
                    self.view()
                        .push(Entry::Session("this session ended".to_owned()));
                } else {
                    self.panes.remove(&id);
                }
            }
            // The alate's own, so it is drawn wherever the window happens to be
            // looking.
            Frame::Heartbeat { at, note } => {
                if self.show_log {
                    self.view().push(Entry::Heartbeat { at, note });
                }
            }
            // A permission question is the daemon's and not a conversation's:
            // it reaches every client, so whoever is at a keyboard can answer
            // without first finding the session that asked.
            Frame::Confirm {
                id,
                tool,
                summary,
                risk,
            } => {
                self.confirm = Some(Confirm {
                    id,
                    tool,
                    summary,
                    risk,
                });
            }
            // Everything below belongs to a conversation, and is written into
            // that conversation's pane whether or not it is on screen.
            frame => {
                let Some(id) = session else { return };
                let show_log = self.show_log;
                let view = self.view_of(&id);
                match frame {
                    Frame::TurnStarted => {
                        view.clear_streams();
                        if mine {
                            self.status.running = true;
                        }
                    }
                    Frame::Text { text } => view.push_text(&text),
                    Frame::Thinking { text } => view.push_thinking(&text),
                    Frame::ToolStreamStart { block, name } => view.begin_stream(block, &name),
                    Frame::ToolStreamDelta { block, bytes } => view.count_stream(block, bytes),
                    Frame::ToolCall {
                        id,
                        name,
                        arguments,
                    } => view.push_call(&id, &name, &arguments),
                    Frame::ToolProgress { id, chunk } => view.push_progress(&id, &chunk),
                    Frame::ToolResult {
                        id,
                        text,
                        is_error,
                        details,
                        ..
                    } => view.finish_call(&id, &text, is_error, details),
                    Frame::TurnEnded { usage, error, .. } => {
                        // This runs after every call and result of the turn, so
                        // whatever is still streaming is a call that never came.
                        view.settle_streams();
                        if let Some(error) = error {
                            view.push(Entry::Notice(format!("error: {error}")));
                        }
                        if mine {
                            self.status.last = Some(usage);
                            self.status.total += usage;
                        }
                    }
                    Frame::RunEnded { .. } => {
                        if mine {
                            self.status.running = false;
                        }
                    }
                    Frame::Prompt { text } => view.push(Entry::User(text)),
                    Frame::Notice { text } if show_log => view.push(Entry::Notice(text)),
                    // A frame this build has no name for. A daemon one version
                    // ahead is not a reason to stop drawing.
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aphid_core::StopReason;

    fn model() -> Model {
        let mut model = Model::new("work");
        model.arrived(Envelope::from(
            "s1",
            Frame::Hello {
                instance: "work".to_owned(),
                model: "deepseek-chat".to_owned(),
                context_window: 64_000,
                thinking: None,
            },
        ));
        model
    }

    fn entries(model: &Model, id: &str) -> Vec<Entry> {
        model
            .pane_of(id)
            .map(|pane| pane.entries().to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn the_greeting_names_the_session_this_window_was_given() {
        let model = model();
        assert_eq!(model.current(), "s1");
        assert_eq!(model.instance, "work");
        assert_eq!(model.status.context_window, 64_000);
        assert_eq!(model.link, Link::Connected);
    }

    #[test]
    fn streamed_text_is_one_message_and_not_four() {
        let mut model = model();
        model.arrived(Envelope::from("s1", Frame::TurnStarted));
        for piece in ["A ", "sentence ", "in ", "pieces."] {
            model.arrived(Envelope::from(
                "s1",
                Frame::Text {
                    text: piece.to_owned(),
                },
            ));
        }
        assert_eq!(
            entries(&model, "s1"),
            vec![Entry::Assistant("A sentence in pieces.".to_owned())]
        );
    }

    #[test]
    fn a_tool_call_keeps_its_arguments_as_a_value() {
        let mut model = model();
        model.arrived(Envelope::from("s1", Frame::TurnStarted));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolCall {
                id: "c1".to_owned(),
                name: "read".to_owned(),
                arguments: r#"{"path":"src/lib.rs"}"#.to_owned(),
            },
        ));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolResult {
                id: "c1".to_owned(),
                name: "read".to_owned(),
                text: "fn main() {}".to_owned(),
                is_error: false,
                details: None,
            },
        ));
        let Some(Entry::Tool(tool)) = entries(&model, "s1").into_iter().next() else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.state, ToolState::Done);
        assert_eq!(
            tool.arguments.and_then(|json| json
                .get("path")
                .and_then(|path| path.as_str().map(ToOwned::to_owned))),
            Some("src/lib.rs".to_owned())
        );
        assert_eq!(tool.output, "fn main() {}");
    }

    #[test]
    fn a_streamed_call_lands_in_the_placeholder_its_bytes_filled() {
        let mut model = model();
        model.arrived(Envelope::from("s1", Frame::TurnStarted));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolStreamStart {
                block: 0,
                name: "edit".to_owned(),
            },
        ));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolStreamDelta {
                block: 0,
                bytes: 120,
            },
        ));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolCall {
                id: "c1".to_owned(),
                name: "edit".to_owned(),
                arguments: "{}".to_owned(),
            },
        ));
        let drawn = entries(&model, "s1");
        assert_eq!(drawn.len(), 1, "the call reuses the placeholder");
        let Some(Entry::Tool(tool)) = drawn.into_iter().next() else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.state, ToolState::Running);
        assert_eq!(tool.streamed, 120);
    }

    #[test]
    fn a_turn_that_ends_mid_stream_settles_the_card_it_left_counting() {
        let mut model = model();
        model.arrived(Envelope::from("s1", Frame::TurnStarted));
        model.arrived(Envelope::from(
            "s1",
            Frame::ToolStreamStart {
                block: 0,
                name: "edit".to_owned(),
            },
        ));
        model.arrived(Envelope::from(
            "s1",
            Frame::TurnEnded {
                usage: Usage::default(),
                stop: StopReason::Stop,
                error: None,
            },
        ));
        let Some(Entry::Tool(tool)) = entries(&model, "s1").into_iter().next() else {
            panic!("expected a tool card");
        };
        assert_eq!(tool.state, ToolState::Failed);
    }

    #[test]
    fn a_replay_clears_the_conversation_it_is_replaying_and_no_other() {
        let mut model = model();
        model.arrived(Envelope::from(
            "s1",
            Frame::Prompt {
                text: "one".to_owned(),
            },
        ));
        model.arrived(Envelope::from(
            "s2",
            Frame::Prompt {
                text: "two".to_owned(),
            },
        ));
        model.arrived(Envelope::from(
            "s2",
            Frame::HistoryStart {
                id: "s2".to_owned(),
            },
        ));
        assert!(model.filling());
        assert_eq!(
            entries(&model, "s1").len(),
            1,
            "the other pane is untouched"
        );
        assert!(entries(&model, "s2").is_empty());
        model.arrived(Envelope::from(
            "s2",
            Frame::HistoryEnd {
                id: "s2".to_owned(),
            },
        ));
        assert!(!model.filling());
    }

    #[test]
    fn another_conversations_run_does_not_move_this_ones_status() {
        let mut model = model();
        model.arrived(Envelope::from("s2", Frame::TurnStarted));
        assert!(!model.status.running, "s2 is not what is on screen");
        model.arrived(Envelope::from("s1", Frame::TurnStarted));
        assert!(model.status.running);
        model.arrived(Envelope::from(
            "s1",
            Frame::RunEnded {
                stop: StopReason::Stop,
                turns: 1,
                error: None,
            },
        ));
        assert!(!model.status.running);
    }

    #[test]
    fn a_permission_question_arrives_whichever_session_asked() {
        let mut model = model();
        model.arrived(Envelope::from(
            "s2",
            Frame::Confirm {
                id: 7,
                tool: "bash".to_owned(),
                summary: "rm -rf /".to_owned(),
                risk: Risk::Destructive,
            },
        ));
        assert_eq!(model.confirm.as_ref().map(|confirm| confirm.id), Some(7));
        assert_eq!(
            model.answer(Answer::Deny),
            vec![Request::Answer {
                id: 7,
                decision: Answer::Deny
            }]
        );
        assert!(model.confirm.is_none(), "answering closes the question");
    }

    #[test]
    fn a_frame_this_build_has_no_name_for_is_ignored_in_silence() {
        let mut model = model();
        let line = r#"{"session":"s1","kind":"future_frame","wings":2}"#;
        // What `Client::recv` does with it: an envelope that does not parse is
        // skipped. This is the other half — one that parses into no frame.
        assert!(serde_json::from_str::<Envelope>(line).is_err());
        model.arrived(Envelope::from(
            "s1",
            Frame::Text {
                text: "still drawing".to_owned(),
            },
        ));
        assert_eq!(entries(&model, "s1").len(), 1);
    }

    #[test]
    fn the_log_switch_hides_notices_and_shows_them_again() {
        let mut model = model();
        assert!(model.command("log").is_empty());
        assert!(!model.show_log);
        model.arrived(Envelope::from(
            "s1",
            Frame::Notice {
                text: "a plugin said something".to_owned(),
            },
        ));
        // Only the line the switch itself wrote.
        assert_eq!(entries(&model, "s1").len(), 1);
        let _ = model.command("log");
        model.arrived(Envelope::from(
            "s1",
            Frame::Notice {
                text: "and again".to_owned(),
            },
        ));
        assert_eq!(entries(&model, "s1").len(), 3);
    }

    #[test]
    fn a_slash_command_is_not_sent_to_the_agent() {
        let mut model = model();
        assert_eq!(model.submit("/sessions"), vec![Request::Sessions]);
        assert_eq!(model.submit("/new"), vec![Request::New]);
        assert_eq!(
            model.submit("/session abc"),
            vec![Request::Watch {
                id: "abc".to_owned()
            }]
        );
        assert_eq!(model.current(), "abc");
        assert_eq!(
            model.submit("hello"),
            vec![Request::Prompt {
                text: "hello".to_owned()
            }]
        );
    }

    #[test]
    fn a_prompt_is_drawn_when_the_daemon_echoes_it_and_not_before() {
        let mut model = model();
        let _ = model.submit("hello");
        assert!(entries(&model, "s1").is_empty(), "no local echo");
        model.arrived(Envelope::from(
            "s1",
            Frame::Prompt {
                text: "hello".to_owned(),
            },
        ));
        assert_eq!(entries(&model, "s1"), vec![Entry::User("hello".to_owned())]);
    }

    #[test]
    fn a_session_list_is_kept_as_the_sessions_and_not_as_a_line_of_text() {
        let mut model = model();
        let info = Info {
            id: "s9".to_owned(),
            kind: "cron: nightly".to_owned(),
            started: "2026-09-01 20:00".to_owned(),
            running: true,
        };
        model.arrived(Envelope::daemon(Frame::Sessions {
            live: vec![info.clone()],
            stored: Vec::new(),
        }));
        assert_eq!(model.sessions.live, vec![info]);
    }
}
