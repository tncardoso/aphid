//! The context: what a component is handed, and the only way it touches the world.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::effect::{Disposer, Handle};
use super::isolate::{Realm, Realms};
use super::runtime::Inner;
use super::service::{Access, Binding, Service};
use super::uid::Uid;
use crate::tool::BoxFuture;

type Acquire = Box<dyn FnOnce() -> BoxFuture<'static, Disposer> + Send>;

/// Work a component asked for during `apply`, drained by the runtime before the
/// fiber goes active.
///
/// Registrations are recorded rather than acted on so that the whole of an
/// `apply` lands as one step: a dependent cannot observe half of it, and *n*
/// provisions cost one notification pass instead of *n*.
#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) effects: Vec<Handle>,
    pub(crate) acquiring: Vec<Acquire>,
    pub(crate) notify: Vec<&'static str>,
}

/// What a component sees of the runtime it is loaded into.
///
/// Cheap to clone and safe to keep: a closure a component registers may be
/// called from any thread, so this is `Send + Sync` and holds no borrow.
#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Arc<Inner>,
    pub(crate) fiber: Uid,
    pub(crate) realms: Arc<Realms>,
    pub(crate) pending: Arc<Mutex<Pending>>,
}

impl Context {
    /// The fiber this context belongs to.
    #[must_use]
    pub fn uid(&self) -> Uid {
        self.fiber
    }

    /// This component's validated configuration.
    #[must_use]
    pub fn config(&self) -> Value {
        self.inner
            .with_fiber(self.fiber, |fiber| fiber.config.clone())
            .unwrap_or(Value::Null)
    }

    // ---------------------------------------------------------------- effects

    /// Register a revertible effect.
    ///
    /// `body` runs now and returns the inverse; the inverse runs when this
    /// component unloads, after every effect registered later and before every
    /// effect registered earlier.
    pub fn effect(&self, body: impl FnOnce() -> Disposer + Send + 'static) -> Handle {
        let handle = Handle::new(body());
        self.record(handle.clone());
        handle
    }

    /// Register an effect that has to await in order to acquire.
    ///
    /// The body is run by the runtime before the fiber goes active, which is
    /// what keeps [`Component::apply`](super::Component::apply) synchronous
    /// without making the transition synchronous.
    pub fn effect_async<F, Fut>(&self, body: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Disposer> + Send + 'static,
    {
        if let Ok(mut pending) = self.pending.lock() {
            pending.acquiring.push(Box::new(move || Box::pin(body())));
        }
    }

    fn record(&self, handle: Handle) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.effects.push(handle);
        }
    }

    // --------------------------------------------------------------- services

    /// Bind a value under this service's key.
    ///
    /// Provision is an ordinary effect, so the binding is withdrawn when this
    /// component unloads and every dependent is told both times.
    pub fn provide<S: Service>(&self, value: S::Handle) -> Handle {
        let realm = self.realms.realm(S::NAME);
        let erased: Arc<dyn Any + Send + Sync> = Arc::new(value);
        self.inner
            .bind(realm.clone(), Binding::new(self.fiber, erased));

        let inner = Arc::clone(&self.inner);
        let pending = Arc::clone(&self.pending);
        let handle = Handle::new(Disposer::sync(move || {
            inner.unbind(&realm, S::NAME);
        }));
        if let Ok(mut guard) = pending.lock() {
            guard.effects.push(handle.clone());
            guard.notify.push(S::NAME);
        }
        handle
    }

    /// Bind a value under a key known only at runtime.
    ///
    /// The typed [`Context::provide`] is the one to reach for; this exists for
    /// the cases where the key comes from a file rather than from a type — a
    /// script declaring `provides`, or a loader wiring one composition into
    /// another.
    pub fn provide_dyn(&self, key: &'static str, value: Arc<dyn Any + Send + Sync>) -> Handle {
        let realm = self.realms.realm(key);
        self.inner
            .bind(realm.clone(), Binding::new(self.fiber, value));

        let inner = Arc::clone(&self.inner);
        let handle = Handle::new(Disposer::sync(move || {
            inner.unbind(&realm, key);
        }));
        if let Ok(mut guard) = self.pending.lock() {
            guard.effects.push(handle.clone());
            guard.notify.push(key);
        }
        handle
    }

    /// A service this component declared in `inject`.
    ///
    /// # Panics
    ///
    /// If the key is not in this component's `inject`, or if it is and no
    /// provider is active. Both are bugs in the component, and the panic names
    /// which. Use [`Context::get`] to handle them, or [`Context::probe`] for a
    /// capability the component can live without.
    #[must_use]
    pub fn need<S: Service>(&self) -> S::Handle {
        match self.get::<S>() {
            Ok(handle) => handle,
            Err(error) => panic!("{error}"),
        }
    }

    /// A service this component declared, reporting rather than panicking.
    ///
    /// Resolved against this fiber's **committed view** rather than against the
    /// live store, which is what keeps a dependency readable to a component
    /// whose teardown that same dependency triggered.
    ///
    /// # Errors
    ///
    /// [`Access::Undeclared`] when the key is not in `inject`,
    /// [`Access::Inactive`] when it is but nothing provides it.
    pub fn get<S: Service>(&self) -> Result<S::Handle, Access> {
        let mut uid = Some(self.fiber);
        while let Some(current) = uid {
            let found = self.inner.with_fiber(current, |fiber| {
                let bound = fiber
                    .committed
                    .as_ref()
                    .and_then(|view| view.get(S::NAME))
                    .and_then(Binding::downcast::<S>);
                (bound, fiber.inject.contains(&S::NAME), fiber.parent)
            });
            let Some((bound, declared, parent)) = found else {
                break;
            };
            if let Some(handle) = bound {
                return Ok(handle);
            }
            if declared {
                return Err(Access::Inactive(S::NAME));
            }
            uid = parent;
        }
        Err(Access::Undeclared(S::NAME))
    }

    /// A service this component did not declare and can live without.
    ///
    /// Reads the store rather than a committed view, and never fails. Nothing
    /// makes this component reload when the answer changes — that is the trade
    /// for not declaring it.
    #[must_use]
    pub fn probe<S: Service>(&self) -> Option<S::Handle> {
        self.inner
            .lookup(&self.realms.realm(S::NAME))
            .filter(|binding| self.inner.is_active(binding.provider))
            .and_then(|binding| binding.downcast::<S>())
    }

    /// The name-keyed face, for a script bridge or a composition loader.
    #[must_use]
    pub fn probe_dyn(&self, key: &'static str) -> Option<Binding> {
        self.inner
            .lookup(&self.realms.realm(key))
            .filter(|binding| self.inner.is_active(binding.provider))
    }

    // -------------------------------------------------------------- isolation

    /// Derive a context in which `key` resolves to a realm of its own.
    ///
    /// Components mounted on the result see an independent provider of `key`,
    /// and providing it there binds nothing anyone outside can see. Nothing has
    /// to be undone: the parent table was never touched, so discarding the
    /// derived context is the whole of the inverse.
    #[must_use]
    pub fn isolate(&self, key: &'static str) -> Context {
        self.isolate_into(key, Realm::local(key))
    }

    /// Derive a context in which `key` resolves to a **named** realm, shared
    /// with every other context naming it.
    #[must_use]
    pub fn isolate_shared(&self, key: &'static str, realm: impl AsRef<str>) -> Context {
        self.isolate_into(key, Realm::shared(key, realm))
    }

    fn isolate_into(&self, key: &'static str, realm: Realm) -> Context {
        Context {
            inner: Arc::clone(&self.inner),
            fiber: self.fiber,
            realms: self.realms.isolated(key, realm),
            pending: Arc::clone(&self.pending),
        }
    }

    /// This context's realm table.
    #[must_use]
    pub fn realms(&self) -> &Arc<Realms> {
        &self.realms
    }

    // ------------------------------------------------------------ composition

    /// Mount a component as a child of this one.
    ///
    /// Instantiation is an ordinary tracked effect, so unloading this component
    /// unloads what it mounted, recursively.
    pub fn mount(
        &self,
        component: Arc<dyn super::Component>,
        config: Value,
    ) -> Result<Uid, String> {
        let child = self.inner.instantiate(
            Some(self.fiber),
            component,
            config,
            Arc::clone(&self.realms),
        )?;

        let inner = Arc::clone(&self.inner);
        let handle = Handle::new(Disposer::later(move || {
            let inner = Arc::clone(&inner);
            async move { inner.retire(child).await }
        }));
        self.record(handle);
        self.inner.enqueue(child);
        Ok(child)
    }

    pub(crate) fn take_pending(&self) -> Pending {
        self.pending
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    pub(crate) fn view(&self) -> HashMap<&'static str, Binding> {
        self.inner.resolve_view(self.fiber, &self.realms)
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("fiber", &self.fiber)
            .finish()
    }
}
