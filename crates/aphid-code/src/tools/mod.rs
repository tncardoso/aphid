//! The coding agent's tools.
//!
//! Each is a [`ToolHandler`](aphid_agent::ToolHandler) built with
//! [`tool_fn`](aphid_agent::tool_fn). Schemas follow the shapes pi uses, so a
//! prompt written against pi's tools transfers.
//!
//! Every failure comes back as `ToolOutcome::error` rather than a panic: a bad
//! path, a missing file or an ambiguous edit is something the model can read and
//! correct on the next turn.

pub mod bash;
pub mod edit;
pub mod paths;
pub mod read;
pub mod truncate;
pub mod write;

use std::sync::Arc;

use aphid_agent::ToolHandler;
use aphid_agent::rt::Bus;

pub use paths::Workspace;

/// Every tool, ready to register.
///
/// `bus` is where files the tools change are announced; pass `None` when there
/// is nowhere to announce them, which is the case for tests.
/// `processes` is where every command `bash` starts is recorded.
#[must_use]
pub fn all(
    workspace: &Workspace,
    bus: Option<Arc<Bus>>,
    processes: &Arc<aphid_agent::exec::Registry>,
) -> Vec<Arc<dyn ToolHandler>> {
    // Only worth carrying when somebody is actually listening: the handle is
    // cloned into every call otherwise.
    let watcher = bus.filter(|bus| bus.has_listeners::<crate::events::FileChange>());
    vec![
        Arc::new(bash::tool(workspace, processes)),
        Arc::new(read::tool(workspace)),
        Arc::new(write::tool(workspace, watcher.clone())),
        Arc::new(edit::tool(workspace, watcher)),
    ]
}

/// One-line summaries for the system prompt, in the order tools are listed.
#[must_use]
pub fn snippets() -> Vec<(&'static str, &'static str)> {
    vec![
        (bash::NAME, bash::SNIPPET),
        (read::NAME, read::SNIPPET),
        (write::NAME, write::SNIPPET),
        (edit::NAME, edit::SNIPPET),
    ]
}
