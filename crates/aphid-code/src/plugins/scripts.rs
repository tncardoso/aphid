//! Wiring Rhai plugins into the coding harness.
//!
//! [`crate::scripting`] does the loading; this module decides what a script is
//! allowed to do here, and where its `notify` output goes. The two front ends
//! answer that question differently — the terminal UI cannot have anything
//! written under it, and headless has no UI to write to — so each supplies its
//! own [`Sink`].

use std::path::Path;
use std::sync::Arc;

use crate::scripting::trust::{self, Trust};
use aphid_agent::Sink;
use aphid_agent::rt::Bus;

use crate::events::{Ask, Permission};
use crate::scripting::{Capabilities, Diagnostic, PluginFile, PluginHost};

use crate::plugins::permissions::{Confirmer, Decision, Risk};
use crate::tools::Workspace;

/// Find the plugins available to a workspace.
///
/// `home` is where global plugins live; pass `None` to skip them. Both are
/// parameters rather than environment lookups, matching how context files and
/// skills are discovered.
#[must_use]
pub fn discover(workspace: &Workspace, home: Option<&Path>) -> (Vec<PluginFile>, Vec<Diagnostic>) {
    crate::scripting::discover(workspace.root(), home)
}

/// What a plugin may do in a coding session.
///
/// Everything, confined to the workspace. The agent it extends can already run
/// a shell and edit files, so withholding those from a plugin the user installed
/// on purpose would buy no safety — the boundary that matters is which plugins
/// load at all, not what a loaded one may do.
#[must_use]
pub fn capabilities(workspace: &Workspace) -> Capabilities {
    let caps = Capabilities::full(workspace.root());
    match crate::home_dir() {
        Some(home) => caps.with_home(&home),
        None => caps,
    }
}

/// What has to happen before a workspace's own plugins may load.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Gate {
    /// Nothing to decide: either there are no project plugins, or this
    /// workspace was trusted before.
    Open,
    /// Project plugins were found and nobody has been asked yet.
    Ask,
    /// This workspace was refused.
    Refused,
}

/// Whether this workspace's own plugins may load.
///
/// Plugins under the home directory are never gated: they are the user's own,
/// and asking about them every session would teach the user to say yes without
/// reading.
#[must_use]
pub fn gate(workspace: &Workspace, files: &[PluginFile], home: Option<&Path>) -> Gate {
    if !files.iter().any(|file| file.project) {
        return Gate::Open;
    }
    let Some(home) = home else {
        // With nowhere to record an answer, asking every time is worse than not
        // loading: the user cannot make the question go away.
        return Gate::Refused;
    };

    match trust::decision(&trust::path(home), workspace.root()) {
        Trust::Trusted => Gate::Open,
        Trust::Refused => Gate::Refused,
        Trust::Unknown => Gate::Ask,
    }
}

/// Remember what the user said about this workspace.
///
/// # Errors
///
/// Fails when the decisions file cannot be written.
pub fn remember(workspace: &Workspace, home: &Path, trusted: bool) -> std::io::Result<()> {
    trust::record(&trust::path(home), workspace.root(), trusted)
}

/// Drop the workspace's own plugins, keeping the user's.
#[must_use]
pub fn without_project(files: Vec<PluginFile>) -> Vec<PluginFile> {
    files.into_iter().filter(|file| !file.project).collect()
}

/// Load plugins for a workspace.
#[must_use]
pub fn load(
    workspace: &Workspace,
    files: &[PluginFile],
    sink: Arc<dyn Sink>,
    processes: &Arc<aphid_agent::exec::Registry>,
) -> (Arc<PluginHost>, Vec<Diagnostic>) {
    let (host, diagnostics) = PluginHost::load(files, &capabilities(workspace), sink, processes);
    (Arc::new(host), diagnostics)
}

/// Lets a component answer a permission question before the user is asked.
///
/// Wraps another [`Confirmer`] rather than replacing it, so a component decides
/// the cases it has an opinion about and everything else still reaches whoever
/// would have decided anyway. That also means the risk classifier keeps
/// working: a listener sees the summary and risk it produced, not the raw
/// arguments.
pub struct AskFirst {
    bus: Arc<Bus>,
    inner: Arc<dyn Confirmer>,
}

impl AskFirst {
    #[must_use]
    pub fn new(bus: Arc<Bus>, inner: Arc<dyn Confirmer>) -> Self {
        Self { bus, inner }
    }

    /// Wrap `inner` unconditionally.
    ///
    /// Unlike the version this replaces, there is nothing to decide here: what
    /// is subscribed changes while the session runs, so a wrapper installed
    /// only when somebody was listening at startup would be the wrong wrapper
    /// the moment a plugin loaded. Asking an empty bus is one lookup.
    #[must_use]
    pub fn wrap(bus: &Arc<Bus>, inner: Arc<dyn Confirmer>) -> Arc<dyn Confirmer> {
        Arc::new(Self::new(Arc::clone(bus), inner))
    }
}

impl Confirmer for AskFirst {
    fn confirm(&self, tool: &str, summary: &str, risk: Risk) -> Decision {
        let answer = self.bus.bail(&Ask {
            tool: tool.to_owned(),
            summary: summary.to_owned(),
            risk,
        });

        match answer {
            Some(Permission::Allow) => Decision::Allow,
            Some(Permission::AllowAlways) => Decision::AllowAlways,
            Some(Permission::Deny) => Decision::Deny,
            // Nobody had an opinion, so whoever would have decided still does.
            None => self.inner.confirm(tool, summary, risk),
        }
    }
}
