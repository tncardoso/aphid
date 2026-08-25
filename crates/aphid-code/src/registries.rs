//! Where a component puts what it contributes.
//!
//! Three things: `tools` for the model to call, `commands` for a person to
//! type, `surfaces` for a person to look at. All three are **services**, so a
//! component that offers one declares it in `inject` and registers through the
//! context. Registration returns its own inverse, which is what makes the
//! awkward cases go away without anybody handling them:
//!
//! - A component waiting on a service it never gets does not have its `/command`
//!   listed, because it never ran to register it.
//! - Unloading a component takes its command and its panel with it.
//! - Two components offering `/review` is not a collision the loader has to
//!   arbitrate; the second is offered as `/review:2` and both leave when their
//!   owners do.
//!
//! The alternative — reading the contributions off whatever files happened to
//! compile — cannot do any of that, because a compiled file is not a loaded
//! component.
//!
//! One provider rather than three because nothing has ever wanted one without
//! the others: a plugin with a panel usually has a command to open it, and one
//! with a tool usually has a command to invoke it by hand.

use std::sync::{Arc, RwLock};

use aphid_agent::rt::{Context, Disposer, Service, Uid};

use crate::scripting::{CommandSpec, SurfaceSpec};

/// Tools the model may call.
///
/// The handle is the agent's live tool table rather than a registry of its own,
/// because the loop reads it afresh each turn and a second copy would be a
/// second thing to keep true.
pub struct Tools;

impl Service for Tools {
    const NAME: &'static str = "tools";
    type Handle = Arc<aphid_agent::Toolbox>;
}

/// Slash commands, and who offered each.
pub struct Commands;

impl Service for Commands {
    const NAME: &'static str = "commands";
    type Handle = Arc<Registry<Command>>;
}

/// Terminal panels, and who offered each.
pub struct Surfaces;

impl Service for Surfaces {
    const NAME: &'static str = "surfaces";
    type Handle = Arc<Registry<Surface>>;
}

/// One command on offer.
#[derive(Clone)]
pub struct Command {
    pub spec: CommandSpec,
    /// The component that offered it, for diagnostics and for `/plugins`.
    pub source: String,
}

/// One panel on offer.
#[derive(Clone)]
pub struct Surface {
    pub spec: SurfaceSpec,
    pub source: String,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.spec.name)
            .field("source", &self.source)
            .finish()
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("name", &self.spec.name)
            .field("source", &self.source)
            .finish()
    }
}

/// What a component offered, held for as long as it is loaded.
pub struct Registry<T> {
    entries: RwLock<Vec<(Uid, T)>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Registry {
            entries: RwLock::default(),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Registry<T> {
    #[must_use]
    pub fn new() -> Arc<Registry<T>> {
        Arc::default()
    }

    /// Offer something for as long as the calling component is loaded.
    ///
    /// The registration and its inverse in one call, because a component that
    /// writes them apart is one refactor away from writing only the first.
    pub fn contribute(self: &Arc<Self>, ctx: &Context, entry: T) {
        let registry = Arc::clone(self);
        let owner = ctx.uid();
        ctx.effect(move || {
            if let Ok(mut entries) = registry.entries.write() {
                entries.push((owner, entry));
            }
            let holder = Arc::clone(&registry);
            Disposer::sync(move || holder.withdraw(owner))
        });
    }

    /// Everything a component offered, taken back.
    fn withdraw(&self, owner: Uid) {
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|(uid, _)| *uid != owner);
        }
    }

    /// What is on offer right now, in registration order.
    #[must_use]
    pub fn entries(&self) -> Vec<T> {
        self.entries
            .read()
            .map(|entries| entries.iter().map(|(_, entry)| entry.clone()).collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.read().map(|e| e.is_empty()).unwrap_or(true)
    }
}

impl<T> std::fmt::Debug for Registry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map(|e| e.len()).unwrap_or(0);
        f.debug_struct("Registry").field("entries", &count).finish()
    }
}

/// Provides `tools`, `commands` and `surfaces`.
pub struct Registries {
    tools: Arc<aphid_agent::Toolbox>,
    commands: Arc<Registry<Command>>,
    surfaces: Arc<Registry<Surface>>,
}

impl Registries {
    /// Offer the composition's own tool table, so that what a component
    /// registers is what the loop reads.
    #[must_use]
    pub fn new(tools: Arc<aphid_agent::Toolbox>) -> Registries {
        Registries {
            tools,
            commands: Registry::new(),
            surfaces: Registry::new(),
        }
    }

    /// Provide for a composition, taking its tool table.
    #[must_use]
    pub fn for_composition(composition: &aphid_agent::rt::Composition) -> Arc<Registries> {
        Arc::new(Registries::new(Arc::clone(&composition.tools)))
    }

    #[must_use]
    pub fn tools(&self) -> &Arc<aphid_agent::Toolbox> {
        &self.tools
    }

    #[must_use]
    pub fn commands(&self) -> &Arc<Registry<Command>> {
        &self.commands
    }

    #[must_use]
    pub fn surfaces(&self) -> &Arc<Registry<Surface>> {
        &self.surfaces
    }
}

impl aphid_agent::rt::Component for Registries {
    fn name(&self) -> &str {
        "registries"
    }

    fn provides(&self) -> &[&'static str] {
        &["tools", "commands", "surfaces"]
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        ctx.provide::<Tools>(Arc::clone(&self.tools));
        ctx.provide::<Commands>(Arc::clone(&self.commands));
        ctx.provide::<Surfaces>(Arc::clone(&self.surfaces));
        Ok(())
    }
}
