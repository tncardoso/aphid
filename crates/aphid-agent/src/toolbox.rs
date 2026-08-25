//! A tool registry components can add to and take back from.
//!
//! [`Tools`](crate::Tools) is a plain table built once. This wraps one so that
//! registering is a revertible effect: a component that contributes a tool has
//! it removed when the component unloads, and a reloaded component's new tools
//! replace its old ones without anybody diffing anything.
//!
//! That is also what lets the set of tools change inside a session at all. The
//! loop reads the declarations afresh each turn, so a tool added between turns
//! is offered on the next request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use aphid_core::Tool;

use crate::registry::Tools;
use crate::rt::{Context, Disposer, Uid};
use crate::tool::ToolHandler;

/// Tools, and who contributed each.
#[derive(Default)]
pub struct Toolbox {
    inner: RwLock<Inner>,
    /// Bumped on every change, so a caller can tell whether the set it last
    /// looked at is still current without comparing it.
    version: AtomicU64,
}

#[derive(Default)]
struct Inner {
    tools: Tools,
    /// Which fiber contributed each name, so unloading removes exactly its own.
    owners: Vec<(Uid, String)>,
}

impl Toolbox {
    #[must_use]
    pub fn new() -> Toolbox {
        Toolbox::default()
    }

    /// Add a tool with no owner. For the harness's own tools, which live as
    /// long as the agent does.
    ///
    /// A name a component already claimed is **left alone**. Shadowing a
    /// built-in is something components are meant to do, and making it depend
    /// on whether the component happened to mount before the agent was built
    /// would be a rule nobody could rely on.
    pub fn push(&self, handler: Arc<dyn ToolHandler>) {
        if let Ok(mut inner) = self.inner.write() {
            let name = handler.declaration().name.as_str();
            if inner.owners.iter().any(|(_, claimed)| claimed == name) {
                return;
            }
            inner.tools.push(handler);
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Add a tool on a component's behalf, and hand back the inverse.
    ///
    /// A later registration under the same name replaces the earlier one, which
    /// is how a component overrides a built-in.
    pub fn register(self: &Arc<Self>, owner: Uid, handler: Arc<dyn ToolHandler>) -> Disposer {
        let name = handler.declaration().name.to_string();
        if let Ok(mut inner) = self.inner.write() {
            inner.tools.push(handler);
            inner.owners.push((owner, name.clone()));
        }
        self.version.fetch_add(1, Ordering::Release);

        // Holds the box rather than a borrow: the inverse has to work after
        // the component that asked for it is gone.
        let toolbox = Arc::clone(self);
        Disposer::sync(move || toolbox.withdraw_tool(owner, &name))
    }

    /// Contribute a tool for as long as a component is loaded.
    ///
    /// The registration and its inverse in one call, because a component that
    /// writes them apart is one refactor away from writing only the first.
    pub fn contribute(self: &Arc<Self>, ctx: &Context, handler: Arc<dyn ToolHandler>) {
        let toolbox = Arc::clone(self);
        let owner = ctx.uid();
        ctx.effect(move || toolbox.register(owner, handler));
    }

    /// Remove one tool a fiber contributed.
    fn withdraw_tool(&self, owner: Uid, name: &str) {
        if let Ok(mut inner) = self.inner.write() {
            let before = inner.owners.len();
            inner
                .owners
                .retain(|(uid, tool)| !(*uid == owner && tool == name));
            if inner.owners.len() != before {
                inner.tools.remove(name);
            }
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Everything a fiber contributed, removed.
    pub fn withdraw(&self, owner: Uid) {
        if let Ok(mut inner) = self.inner.write() {
            let mine: Vec<String> = inner
                .owners
                .iter()
                .filter(|(uid, _)| *uid == owner)
                .map(|(_, name)| name.clone())
                .collect();
            inner.owners.retain(|(uid, _)| *uid != owner);
            for name in mine {
                inner.tools.remove(&name);
            }
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    /// The declarations, as a request encoder wants them.
    #[must_use]
    pub fn declarations(&self) -> Vec<Tool> {
        self.inner
            .read()
            .map(|inner| inner.tools.declarations().to_vec())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.inner
            .read()
            .ok()
            .and_then(|inner| inner.tools.get(name).cloned())
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.inner
            .read()
            .map(|inner| inner.tools.names().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .map(|inner| inner.tools.len())
            .unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Which revision of the set this is. Changes whenever a tool is added or
    /// removed, and never otherwise.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}
