//! Wiring Rhai plugins into the coding harness.
//!
//! [`aphid_plugin`] does the loading; this module decides what a script is
//! allowed to do here, and where its `notify` output goes. The two front ends
//! answer that question differently — the terminal UI cannot have anything
//! written under it, and headless has no UI to write to — so each supplies its
//! own [`Sink`].

use std::path::Path;
use std::sync::Arc;

use aphid_plugin::{Capabilities, Diagnostic, PluginFile, PluginHost, Sink};

use crate::tools::Workspace;

/// Find the plugins available to a workspace.
///
/// `home` is where global plugins live; pass `None` to skip them. Both are
/// parameters rather than environment lookups, matching how context files and
/// skills are discovered.
#[must_use]
pub fn discover(workspace: &Workspace, home: Option<&Path>) -> (Vec<PluginFile>, Vec<Diagnostic>) {
    aphid_plugin::discover(workspace.root(), home)
}

/// What a plugin may do in a coding session.
///
/// Everything, confined to the workspace. The agent it extends can already run
/// a shell and edit files, so withholding those from a plugin the user installed
/// on purpose would buy no safety — the boundary that matters is which plugins
/// load at all, not what a loaded one may do.
#[must_use]
pub fn capabilities(workspace: &Workspace) -> Capabilities {
    Capabilities::full(workspace.root())
}

/// Load plugins for a workspace.
#[must_use]
pub fn load(
    workspace: &Workspace,
    files: &[PluginFile],
    sink: Arc<dyn Sink>,
) -> (Arc<PluginHost>, Vec<Diagnostic>) {
    let (host, diagnostics) = PluginHost::load(files, &capabilities(workspace), sink);
    (Arc::new(host), diagnostics)
}
