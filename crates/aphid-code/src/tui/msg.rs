//! Everything the terminal reacts to.
//!
//! One type, from four places: the agent's hooks, the plugins, the terminal
//! itself, and the runtime's own timers and reports. All of it plain data — no
//! channel ends, no handles, nothing that has to be kept alive — so a message
//! can be logged, compared, put on a wire, and replayed into a model with no
//! runtime under it.

use aphid_agent::exec;
use aphid_core::{Json, StopReason, Usage};
use ratatui::crossterm::event::{KeyEvent, MouseEvent};

use crate::plugins::permissions::Risk;
use crate::tui::clipboard::Copied;
use crate::tui::runtime::RequestId;

/// What happened.
#[derive(Clone, Debug)]
pub enum Msg {
    // ---- the agent, by way of its hooks -----------------------------------
    TurnStarted,
    /// A chunk of assistant prose.
    Text(String),
    /// A chunk of reasoning.
    Thinking(String),
    /// A tool-call block opened. Its arguments are still arriving, and the call
    /// itself is not announced until the whole turn has been committed.
    ToolStreamStart {
        block: u32,
        name: String,
    },
    /// More argument bytes landed in the block at `block`.
    ToolStreamDelta {
        block: u32,
        bytes: usize,
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
    /// A run finished. Flattened from the outcome to the three fields anything
    /// downstream reads, which is also the shape the alate wire carries.
    RunEnded {
        stop: StopReason,
        turns: u32,
        error: Option<String>,
    },
    /// A run's task did not finish: it panicked or was cancelled outright.
    RunFailed(String),

    // ---- the plugins ------------------------------------------------------
    /// A gated tool is waiting for an answer. The agent's task is blocked on
    /// the channel this id names.
    Confirm {
        id: RequestId,
        tool: String,
        summary: String,
        risk: Risk,
    },
    /// Something a plugin wants the user to see.
    Notice(String),
    /// Text a plugin sent to the model, which is queued as a typed line is.
    Prompt(String),

    // ---- the terminal -----------------------------------------------------
    Key(KeyEvent),
    /// A block of text the terminal delivered in one piece, under bracketed
    /// paste. Its newlines are text, not the Enters they would be if the same
    /// characters had been typed.
    Paste(String),
    /// A mouse button, drag or wheel event. The app decides who it belongs to.
    Mouse(MouseEvent),
    Resize,
    /// What became of a selection the app asked to copy.
    Copied {
        lines: usize,
        outcome: Copied,
    },

    // ---- the runtime ------------------------------------------------------
    /// The repaint tick, while something is streaming.
    Frame,
    /// The slow tick, while a screen showing elapsed time is open.
    Poll,
    /// What the process registry holds, in answer to a poll.
    Processes(Vec<exec::Process>),
    /// A `!` command finished.
    BangOutput {
        command: String,
        output: String,
    },
    /// What the last draw settled: how the pane was wrapped and where the
    /// panels put their clickable regions. The one road from the screen back
    /// into the model, and it is a message like everything else.
    LaidOut(crate::tui::render::Laid),
    /// The background tick.
    Tick,
    /// The panels, freshly rendered by the plugins that own them.
    Panes(crate::tui::surface::Panes),
    /// A surface handled an event and asked for these.
    SurfaceDone {
        plugin: String,
        actions: Vec<crate::scripting::SurfaceAction>,
    },
    /// The UI-neutral trees for the graphical front end's plugin panels.
    PluginSurfaces(Vec<crate::scripting::Open>),
}
