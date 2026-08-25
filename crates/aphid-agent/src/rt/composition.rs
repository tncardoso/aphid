//! What a front end assembles a system out of.
//!
//! Four things travel together — the fiber graph, the event bus, and the two
//! arena-borrow listener lists — because a component needs all four and none of
//! them is useful alone. Passing one value rather than four also means adding a
//! fifth later does not touch every call site.
//!
//! The order this makes possible is the point. A front end builds a
//! composition, mounts components onto it, and only then constructs the agent
//! loop against the same bus — so components are already subscribed when the
//! loop starts announcing, without anybody arranging that by hand.

use std::sync::Arc;

use serde_json::Value;

use super::Bus;
use super::component::Component;
use super::runtime::Runtime;
use super::uid::Uid;
use crate::events::{StreamListeners, TranscriptListeners};
use crate::toolbox::Toolbox;

/// A system being assembled.
#[derive(Clone)]
pub struct Composition {
    pub runtime: Runtime,
    pub bus: Arc<Bus>,
    /// Per-token subscribers. Deliberately not on the bus; see
    /// [`StreamListeners`].
    pub stream: Arc<StreamListeners>,
    /// Subscribers that read the transcript where it grew. Also not on the bus.
    pub transcript: Arc<TranscriptListeners>,
    /// The tools on offer. Registering is revertible, so a component's tools
    /// leave with it and the set can change inside a session.
    pub tools: Arc<Toolbox>,
}

impl Default for Composition {
    fn default() -> Self {
        Composition::new()
    }
}

impl Composition {
    #[must_use]
    pub fn new() -> Composition {
        Composition {
            runtime: Runtime::new(),
            bus: Arc::new(Bus::new()),
            stream: Arc::new(StreamListeners::new()),
            transcript: Arc::new(TranscriptListeners::new()),
            tools: Arc::new(Toolbox::new()),
        }
    }

    /// Mount a component without waiting for it to load.
    ///
    /// Loading needs to await, and plenty of assembly code is not async. This
    /// records the component and leaves it queued; the agent loop settles the
    /// composition before it announces anything, so a component mounted this
    /// way is always in place before the first thing it could miss.
    ///
    /// # Errors
    ///
    /// The component's own refusal: configuration that fails its schema, or a
    /// dependency cycle it would close. Both are known without loading.
    pub fn mount(&self, component: Arc<dyn Component>, config: Value) -> Result<Uid, String> {
        self.runtime.mount(component, config)
    }

    /// Mount a component and run the composition to quiescence.
    ///
    /// # Errors
    ///
    /// The component's own refusal: configuration that fails its schema, or a
    /// dependency cycle it would close.
    pub async fn add(&self, component: Arc<dyn Component>, config: Value) -> Result<Uid, String> {
        let uid = self.runtime.mount(component, config)?;
        self.runtime.settle().await;
        Ok(uid)
    }

    /// Mount a component with no configuration.
    ///
    /// # Errors
    ///
    /// As [`Composition::add`].
    pub async fn plug(&self, component: impl Component) -> Result<Uid, String> {
        self.add(Arc::new(component), Value::Null).await
    }

    /// Unload everything, reverting each component's effects.
    pub async fn shutdown(&self) {
        self.runtime.shutdown().await;
    }
}

impl std::fmt::Debug for Composition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composition")
            .field("fibers", &self.runtime.roster().len())
            .field("bus", &self.bus)
            .finish()
    }
}
