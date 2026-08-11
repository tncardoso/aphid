//! What crosses the socket.
//!
//! One JSON object for each line, in both directions. A line is a frame, which
//! makes the protocol readable with `nc` and makes `alate.log` — the same
//! frames, appended — readable with `jq`.
//!
//! The daemon's frames are the owned parts of [`UiEvent`], plus the two things
//! a resident agent has and a terminal session does not: a heartbeat, and a
//! status a client that attached late needs before it can draw anything.
//!
//! [`UiEvent`]: aphid_code::tui::UiEvent

use aphid_code::plugins::permissions::{Decision, Risk as PermissionRisk};
use aphid_core::{Json, StopReason, Usage};
use serde::{Deserialize, Serialize};

/// What a client asks of the daemon.
///
/// A request needs no session on it: a connection has a session it is watching,
/// and everything it asks for is about that one. `Watch` is what changes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// I am a client, not a probe. Sent by [`Client::connect`] before anything
    /// else; the daemon opens a session and greets it on this.
    ///
    /// Connecting is not enough, because `is_listening` connects too, and a
    /// check that an alate is awake must not leave a conversation behind it.
    ///
    /// `channel` is what the client says it is — `telegram: 42`, and a terminal
    /// says nothing. It becomes the name of the session in a listing, so that a
    /// list of conversations says where each one is being had. It is absent on
    /// the wire when there is none, which keeps `{"kind":"attach"}` the whole
    /// handshake for anything written by hand.
    ///
    /// [`Client::connect`]: super::Client::connect
    Attach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    /// Say this to the agent, as though it had been typed.
    Prompt { text: String },
    /// Stop the run in flight.
    Cancel,
    /// The answer to a [`Frame::Confirm`]. The first answer for an id wins;
    /// later ones find nothing waiting and are dropped.
    Answer { id: u64, decision: Answer },
    /// Watch this session instead. It is replayed from the beginning, whether
    /// it is running or long finished on disk, and then followed live.
    Watch { id: String },
    /// What sessions are there. Answered to this connection alone.
    Sessions,
    /// Open another session on this connection.
    New,
}

/// What a client said about a tool that asked permission.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Allow,
    /// Allow this, and anything identical, until the daemon stops.
    AllowAlways,
    Deny,
}

impl From<Answer> for Decision {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Allow => Decision::Allow,
            Answer::AllowAlways => Decision::AllowAlways,
            Answer::Deny => Decision::Deny,
        }
    }
}

/// How much damage a tool could do.
///
/// A copy of [`PermissionRisk`], which has no serde derives and should not grow
/// them for one caller's sake.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Read,
    Mutate,
    Destructive,
}

impl From<PermissionRisk> for Risk {
    fn from(risk: PermissionRisk) -> Self {
        match risk {
            PermissionRisk::Read => Risk::Read,
            PermissionRisk::Mutate => Risk::Mutate,
            PermissionRisk::Destructive => Risk::Destructive,
        }
    }
}

impl From<Risk> for PermissionRisk {
    fn from(risk: Risk) -> Self {
        match risk {
            Risk::Read => PermissionRisk::Read,
            Risk::Mutate => PermissionRisk::Mutate,
            Risk::Destructive => PermissionRisk::Destructive,
        }
    }
}

/// One frame, and which conversation it belongs to.
///
/// Every line the daemon writes is one of these. `session` is `None` when the
/// daemon is speaking for itself rather than for a conversation — the greeting,
/// a session list, a note about start-up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(flatten)]
    pub frame: Frame,
}

impl Envelope {
    /// A frame from a conversation.
    #[must_use]
    pub fn from(session: impl Into<String>, frame: Frame) -> Self {
        Self {
            session: Some(session.into()),
            frame,
        }
    }

    /// A frame from the daemon itself.
    #[must_use]
    pub fn daemon(frame: Frame) -> Self {
        Self {
            session: None,
            frame,
        }
    }

    /// Whether a connection watching `current` should be shown this.
    ///
    /// The daemon's own frames go to everybody. A conversation's frames go only
    /// to the connections looking at it, which is what keeps two terminals on
    /// two sessions from drawing each other's replies.
    #[must_use]
    pub fn is_for(&self, current: Option<&str>) -> bool {
        match &self.session {
            None => true,
            Some(session) => current == Some(session.as_str()),
        }
    }
}

/// What the daemon tells its clients.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// The first frame a connection is sent: what the alate is. The session
    /// opened for this terminal is the one the envelope names, so it is not
    /// repeated here — a second `session` field would collide with the
    /// envelope's own when the frame is flattened into it.
    Hello {
        instance: String,
        model: String,
        context_window: u32,
        thinking: Option<String>,
    },
    /// A session started. Sent to everybody, so a terminal can see a job wake
    /// up without being in it.
    SessionOpened {
        info: crate::sessions::Info,
    },
    /// A session ended and will send nothing more.
    SessionClosed {
        id: String,
    },
    /// The sessions there are. An answer to [`Request::Sessions`], and sent to
    /// the connection that asked and to nobody else.
    Sessions {
        /// Running now.
        live: Vec<crate::sessions::Info>,
        /// Finished, and on disk.
        stored: Vec<crate::sessions::Info>,
    },
    /// A replay of a session is starting. Whatever a client has drawn for this
    /// session is stale; clear it and take what follows.
    HistoryStart {
        id: String,
    },
    /// The replay is done. What comes next is live.
    HistoryEnd {
        id: String,
    },
    TurnStarted,
    Text {
        text: String,
    },
    /// A tool call opened and its arguments are still arriving. Carried so a
    /// client can show that a slow call is moving; the call itself is announced
    /// once the whole turn is committed.
    ToolStreamStart {
        block: u32,
        name: String,
    },
    ToolStreamDelta {
        block: u32,
        bytes: usize,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolProgress {
        id: String,
        chunk: String,
    },
    ToolResult {
        id: String,
        name: String,
        text: String,
        is_error: bool,
        details: Option<Json>,
    },
    TurnEnded {
        usage: Usage,
        stop: StopReason,
        error: Option<String>,
    },
    RunEnded {
        stop: StopReason,
        turns: u32,
        error: Option<String>,
    },
    /// Something a plugin wants seen.
    Notice {
        text: String,
    },
    /// A prompt went to the agent, from a client or from a plugin. Echoed to
    /// everybody watching that session, so two terminals on one session agree.
    Prompt {
        text: String,
    },
    /// The alate woke on its own, and the line it woke to.
    ///
    /// The daemon's, not the resident session's, so it reaches every terminal:
    /// somebody in a conversation of their own still wants to see that the
    /// alate is stirring. A job needs no frame like this, because
    /// [`Frame::SessionOpened`] already names it and the session it opened.
    Heartbeat {
        at: String,
        note: String,
    },
    /// A tool is waiting for an answer. Sent to every client; the first
    /// [`Request::Answer`] decides.
    Confirm {
        id: u64,
        tool: String,
        summary: String,
        risk: Risk,
    },
}

impl Frame {
    /// Whether this frame is worth keeping for a client that attaches later.
    ///
    /// A confirmation is not: by the time anybody reads the backlog the tool
    /// has been answered or has timed out, and replaying it would open a modal
    /// over a question nobody can still answer. Neither is anything addressed
    /// to one connection — a greeting, a session list, or a replay somebody
    /// else asked for.
    #[must_use]
    pub fn is_history(&self) -> bool {
        !matches!(
            self,
            Frame::Confirm { .. }
                | Frame::Hello { .. }
                | Frame::Sessions { .. }
                | Frame::HistoryStart { .. }
                | Frame::HistoryEnd { .. }
        )
    }
}
