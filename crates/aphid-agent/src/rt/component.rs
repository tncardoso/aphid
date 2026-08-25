//! What a component is.

use serde_json::Value;

use super::context::Context;

/// A unit of composition: it declares what it needs, what it offers, and what
/// it does when both line up.
///
/// # Why `apply` is synchronous
///
/// A component that has to await in order to acquire something wraps that in
/// [`Context::effect_async`](super::Context::effect_async), which the runtime
/// drains before the fiber goes `ACTIVE`. The transition is genuinely async;
/// `apply` itself stays a function you can read top to bottom.
pub trait Component: Send + Sync + 'static {
    /// Used in diagnostics and in the composition file.
    fn name(&self) -> &str;

    /// The service keys this component requires. It stays `PENDING` until every
    /// one of them is provided by an active fiber, and unloads again if one
    /// goes away.
    ///
    /// Read before `apply` and never during: a coeffect specification is
    /// static, which is what lets the runtime decide when to run this at all.
    fn inject(&self) -> &[&'static str] {
        &[]
    }

    /// The service keys this component intends to provide.
    ///
    /// Declared rather than discovered so that a dependency cycle is a
    /// diagnostic at mount rather than two components that quietly never load.
    fn provides(&self) -> &[&'static str] {
        &[]
    }

    /// The events this component emits, so that subscribing to a name nobody
    /// emits is reported rather than silently never firing.
    fn emits(&self) -> &[&'static str] {
        &[]
    }

    /// A JSON Schema for this component's configuration. Config that fails it
    /// puts the fiber in `FAILED` with the offending field named, rather than
    /// letting it start half-configured.
    fn schema(&self) -> Option<&Value> {
        None
    }

    /// Contribute. Everything registered through `ctx` is reverted when this
    /// fiber unloads.
    ///
    /// # Errors
    ///
    /// A message for the operator. The fiber goes `FAILED` and stays down.
    fn apply(&self, ctx: &Context) -> Result<(), String>;
}
