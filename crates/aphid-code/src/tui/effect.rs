//! Everything the terminal wants done.
//!
//! An update returns these instead of doing them. They are plain values, so a
//! test reads what a keypress decided without an agent, a plugin host or a
//! terminal anywhere near it.

use aphid_core::{Model, ThinkingLevel};
use aphid_plugin::SurfaceEvent;

use crate::plugins::permissions::Decision;
use crate::tui::runtime::RequestId;

/// One thing for the executor to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Send this prompt to the agent. The executor takes the idle agent, runs
    /// it on a task of its own, and reports back with
    /// [`Msg::RunEnded`](crate::tui::msg::Msg::RunEnded).
    StartRun(String),
    /// Stop whatever the agent is doing.
    Cancel,
    /// Point the agent at another model, credentials and all.
    ///
    /// A switch asked for while the agent is away is applied when it comes
    /// back, rather than dropped as it used to be.
    SetModel(Box<Model>),
    SetThinking(Option<ThinkingLevel>),
    /// Drop the conversation from the agent's transcript, keeping the system
    /// prompt. The pane clears itself; this is the other half.
    ClearTranscript,
    /// Run a `!` command in the workspace root.
    Bang(String),
    /// Ask a process to stop.
    Kill(u32),
    /// What the process list is showing is stale.
    SnapshotProcesses,
    /// Answer a question a gated tool call is blocked on.
    Answer {
        id: RequestId,
        decision: Decision,
    },
    /// Run a plugin's slash command.
    PluginCommand {
        name: String,
        args: String,
    },
    /// Tell the plugins what the user was shown.
    PluginNotice(String),
    /// Deliver an event to a plugin's surface.
    Surface {
        plugin: String,
        name: String,
        event: SurfaceEvent,
    },
    /// Redraw every surface whose state has moved on.
    RefreshSurfaces,
    /// Run the plugins' tick.
    PluginTick,
    /// Release every question, cancel the run, and stop.
    Quit,
}
