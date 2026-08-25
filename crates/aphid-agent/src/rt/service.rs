//! Services: the named capabilities components share.
//!
//! A service is a capability one component provides and others consume by
//! **name** rather than by importing its provider, so a composition can choose
//! an implementation without the consumers knowing.
//!
//! # Why the handle is the erased half
//!
//! Every capability in this harness is a trait object — `Arc<dyn Sink>`,
//! `Arc<dyn Confirmer>`, `Arc<dyn Backend>` — and `Arc<dyn Any>` can only hold
//! something `Sized`. So the store erases the *handle*, which is `Sized` even
//! when what it points at is not, and the marker type names both the key and
//! the handle:
//!
//! ```
//! # use std::sync::Arc;
//! use aphid_agent::rt::Service;
//! use aphid_agent::Sink;
//!
//! pub struct Sinks;
//!
//! impl Service for Sinks {
//!     const NAME: &'static str = "sink";
//!     type Handle = Arc<dyn Sink>;
//! }
//! ```
//!
//! A marker struct also sidesteps the orphan rule. `impl Service for
//! Arc<dyn Confirmer>` in a downstream crate would be a coherence violation —
//! `Service` is foreign there, and `Arc` is not `#[fundamental]`, so
//! `Arc<LocalType>` is not a local type. A marker struct unambiguously is.

use std::any::Any;
use std::sync::Arc;

use super::uid::Uid;

/// A capability, named once and typed once.
///
/// The implementing type is a marker: it is never instantiated, it only fixes
/// the key and the handle consumers receive.
pub trait Service: 'static {
    /// One flat namespace per application, as in Cordis. Prefix your own.
    const NAME: &'static str;

    /// What a consumer gets. Usually `Arc<Self>` for a concrete service; a
    /// trait-object capability names `Arc<dyn Trait>` here, which is what lets
    /// `dyn` bindings work at all.
    type Handle: Clone + Send + Sync + 'static;
}

/// A bound value, and who bound it.
#[derive(Clone)]
pub struct Binding {
    /// The fiber whose activation installed this. Identity by provider rather
    /// than by value is what makes a target comparison sufficient.
    pub(crate) provider: Uid,
    pub(crate) value: Arc<dyn Any + Send + Sync>,
}

impl Binding {
    pub(crate) fn new(provider: Uid, value: Arc<dyn Any + Send + Sync>) -> Binding {
        Binding { provider, value }
    }

    /// The fiber that installed this binding.
    #[must_use]
    pub fn provider(&self) -> Uid {
        self.provider
    }

    pub(crate) fn downcast<S: Service>(&self) -> Option<S::Handle> {
        self.value.downcast_ref::<S::Handle>().cloned()
    }

    /// The bound value, for a consumer that knows the type without knowing the
    /// [`Service`] that named it — a script bridge, or a loader.
    #[must_use]
    pub fn value<T: Clone + 'static>(&self) -> Option<T> {
        self.value.downcast_ref::<T>().cloned()
    }
}

impl std::fmt::Debug for Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Binding")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// Why a declared access did not resolve.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Access {
    /// Declared in `inject`, but no active provider — so this component is not
    /// loaded, and reaching the access at all is a bug in its own teardown.
    Inactive(&'static str),
    /// Not declared in `inject`. The runtime form of a check a compiler could
    /// make: the coeffect specification is static, so this is detectable before
    /// the component runs.
    Undeclared(&'static str),
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Access::Inactive(key) => write!(f, "service `{key}` is declared but not active"),
            Access::Undeclared(key) => write!(f, "service `{key}` was never declared in `inject`"),
        }
    }
}

impl std::error::Error for Access {}
