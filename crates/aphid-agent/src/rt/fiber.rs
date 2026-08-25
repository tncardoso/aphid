//! A fiber: one component, instantiated.

use std::collections::HashMap;
use std::sync::Arc;

use super::component::Component;
use super::effect::Handle;
use super::isolate::Realms;
use super::service::Binding;
use super::uid::Uid;

/// Where a fiber is in its life.
///
/// `PENDING` and `INACTIVE` are the same state to the algorithm — neither is
/// loaded, both are waiting for their declared keys to resolve. They are told
/// apart only so that the answer to "why is my plugin doing nothing?" is a
/// word rather than an inference.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum State {
    /// Declared, but a required service has never been available.
    Pending,
    /// `apply` is running.
    Loading,
    /// `apply` finished and its effects are in place.
    Active,
    /// Disposers are running. A fiber here has already **stopped providing**,
    /// which is what lets its dependents tear down while its bindings still
    /// stand.
    Unloading,
    /// `apply` raised, or config failed validation. Stays down until something
    /// changes.
    Failed,
    /// Was loaded, is not any more.
    Inactive,
}

impl State {
    #[must_use]
    pub fn is_loaded(self) -> bool {
        matches!(self, State::Active)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Loading => "loading",
            State::Active => "active",
            State::Unloading => "unloading",
            State::Failed => "failed",
            State::Inactive => "inactive",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which fibers currently satisfy this one's declared keys.
///
/// The provider uids in `inject` order — not a hash of them. A digest
/// collision here is a reload that silently does not happen, and there is no
/// symptom to debug. Comparison is a few words.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Target(pub(crate) Vec<Uid>);

pub(crate) struct Fiber {
    pub(crate) uid: Uid,
    pub(crate) name: String,
    pub(crate) parent: Option<Uid>,
    pub(crate) children: Vec<Uid>,
    pub(crate) component: Arc<dyn Component>,
    pub(crate) config: serde_json::Value,
    pub(crate) realms: Arc<Realms>,
    pub(crate) inject: Vec<&'static str>,
    pub(crate) provides: Vec<&'static str>,
    /// `None` is ⊥: not satisfiable, so not loaded.
    pub(crate) target: Option<Target>,
    /// The view resolved when loading began, discarded only after every
    /// inverse has run — which is what keeps a dependency readable to a
    /// component whose teardown that same dependency triggered.
    pub(crate) committed: Option<HashMap<&'static str, Binding>>,
    pub(crate) state: State,
    /// LIFO. Reverted back to front.
    pub(crate) dispose: Vec<Handle>,
    /// Whether a transition is already in flight for this fiber. Guards
    /// re-entry; see the note on recursion in [`lifecycle`](super::lifecycle).
    pub(crate) transitioning: bool,
    pub(crate) error: Option<String>,
    /// Whether the composition has switched this off. Distinct from ⊥: a
    /// disabled fiber is unsatisfiable by decree rather than by dependency.
    pub(crate) disabled: bool,
}

impl Fiber {
    pub(crate) fn new(
        uid: Uid,
        parent: Option<Uid>,
        component: Arc<dyn Component>,
        config: serde_json::Value,
        realms: Arc<Realms>,
    ) -> Fiber {
        Fiber {
            uid,
            name: component.name().to_owned(),
            parent,
            children: Vec::new(),
            inject: component.inject().to_vec(),
            provides: component.provides().to_vec(),
            component,
            config,
            realms,
            target: None,
            committed: None,
            state: State::Pending,
            dispose: Vec::new(),
            transitioning: false,
            error: None,
            disabled: false,
        }
    }
}

/// What a fiber looks like from outside the runtime.
#[derive(Clone, Debug)]
pub struct Status {
    pub uid: Uid,
    pub name: String,
    pub state: State,
    pub parent: Option<Uid>,
    pub inject: Vec<&'static str>,
    pub provides: Vec<&'static str>,
    /// Which declared keys have no active provider. Empty unless the fiber is
    /// waiting, and the first thing to look at when it is.
    pub missing: Vec<&'static str>,
    pub error: Option<String>,
}
