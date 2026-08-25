//! How a script reaches a service, and how a service reaches a script.
//!
//! Services are typed, and a script is not. The two meet at a **facade**: a
//! named set of callable methods with `Dynamic` on both sides, which is the
//! same boundary [`convert`](super::convert) already draws between Rhai values
//! and JSON.
//!
//! A facade is **optional** on purpose. A Rust service with none is invisible
//! to scripts — `ctx.call("processes", ...)` on it raises rather than silently
//! doing nothing. That is the same capability boundary
//! [`Capabilities`](super::Capabilities) draws for the filesystem and the
//! shell: a script can reach what it was given and nothing else.

use std::sync::Arc;

use rhai::{Array, Dynamic, FnPtr, Map};

use super::script::ScriptPlugin;

/// How a script may call a Rust service.
pub trait Facade: Send + Sync + 'static {
    /// Call one method. `Err` is a message the script sees as a runtime error,
    /// so a failed call is recoverable rather than fatal.
    ///
    /// # Errors
    ///
    /// Whatever the service wants the script to read: an unknown method, bad
    /// arguments, or a failure of the underlying operation.
    fn call(&self, method: &str, args: Array) -> Result<Dynamic, String>;

    /// The method names, for diagnostics and for `/plugins`.
    fn methods(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A service a script provides: a map of names to functions.
///
/// The owning plugin is held rather than borrowed, because a consumer may call
/// this long after the call that registered it returned — and because the call
/// has to run in the engine the functions were compiled into.
pub struct ScriptService {
    owner: Arc<ScriptPlugin>,
    methods: Map,
}

impl ScriptService {
    #[must_use]
    pub fn new(owner: Arc<ScriptPlugin>, methods: Map) -> Self {
        Self { owner, methods }
    }

    /// Which plugin is behind this.
    #[must_use]
    pub fn provider(&self) -> &str {
        self.owner.name()
    }
}

impl Facade for ScriptService {
    fn call(&self, method: &str, args: Array) -> Result<Dynamic, String> {
        let entry = self
            .methods
            .get(method)
            .ok_or_else(|| format!("`{}` has no method `{method}`", self.provider()))?;
        let function = entry
            .clone()
            .try_cast::<FnPtr>()
            .ok_or_else(|| format!("`{}`.{method} is not a function", self.provider()))?;

        self.owner.call_fn(&function, args)
    }

    fn methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self.methods.keys().map(ToString::to_string).collect();
        names.sort_unstable();
        names
    }
}

impl std::fmt::Debug for ScriptService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptService")
            .field("provider", &self.provider())
            .field("methods", &self.methods())
            .finish()
    }
}
