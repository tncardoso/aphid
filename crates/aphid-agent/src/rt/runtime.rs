//! The runtime: the fiber graph, and the transitions over it.
//!
//! # One writer
//!
//! Every mutation — a transition, a provision, a withdrawal, a mount — is
//! serialised. That is not convenience: the ordering the model rests on
//! assumes transitions do not interleave arbitrarily. A provider must be able
//! to leave service, let its dependents tear down against that, and only then
//! run its own inverses. Interleave two of those and the guarantee is gone.
//!
//! Reads are not serialised. A committed view is resolved once when a fiber
//! loads and read from any thread afterwards, so consuming a service costs a
//! lookup and no coordination.
//!
//! # Why the transitions recurse rather than spawn
//!
//! Unloading a provider has to wait for its dependents to finish unloading
//! first. Cordis spawns each transition as a task and awaits a latch; here the
//! writer is serialised already, so a dependent can only be mid-transition if
//! it is an ancestor of the current one — which is a cycle, and cycles are
//! refused at mount. Awaiting the dependent directly is therefore the same
//! ordering with none of the machinery.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value;

use super::component::Component;
use super::context::{Context, Pending};
use super::effect::{Handle, unwind};
use super::fiber::{Fiber, State, Status, Target};
use super::isolate::{Realm, Realms};
use super::service::Binding;
use super::uid::Uid;
use crate::tool::BoxFuture;

/// One key's journey during a realm reassignment.
struct Move {
    key: &'static str,
    from: Realm,
    to: Realm,
    /// Whether whoever provides the key belongs to the scope being moved. If
    /// so, the binding is that scope's own and travels with it.
    provider_is_ours: bool,
}

pub(crate) struct Inner {
    fibers: Mutex<HashMap<Uid, Fiber>>,
    /// σ — realm to bound value.
    store: RwLock<HashMap<Realm, Binding>>,
    /// key → the fibers injecting it. Built at mount, so notification never
    /// walks the whole graph.
    pub(crate) by_key: Mutex<HashMap<&'static str, Vec<Uid>>>,
    queue: Mutex<VecDeque<Uid>>,
}

impl Inner {
    fn new() -> Arc<Inner> {
        Arc::new(Inner {
            fibers: Mutex::new(HashMap::new()),
            store: RwLock::new(HashMap::new()),
            by_key: Mutex::new(HashMap::new()),
            queue: Mutex::new(VecDeque::new()),
        })
    }

    pub(crate) fn with_fiber<T>(&self, uid: Uid, f: impl FnOnce(&Fiber) -> T) -> Option<T> {
        self.fibers.lock().ok()?.get(&uid).map(f)
    }

    fn with_fiber_mut<T>(&self, uid: Uid, f: impl FnOnce(&mut Fiber) -> T) -> Option<T> {
        self.fibers.lock().ok()?.get_mut(&uid).map(f)
    }

    /// Whether `candidate` is `ancestor`, or was mounted somewhere beneath it.
    ///
    /// The runtime form of "derived from that context": a child fiber's context
    /// is derived from its parent's, all the way up.
    pub(crate) fn descends_from(&self, candidate: Uid, ancestor: Uid) -> bool {
        let mut current = Some(candidate);
        while let Some(uid) = current {
            if uid == ancestor {
                return true;
            }
            current = self.with_fiber(uid, |fiber| fiber.parent).flatten();
        }
        false
    }

    pub(crate) fn is_active(&self, uid: Uid) -> bool {
        self.with_fiber(uid, |fiber| fiber.state.is_loaded())
            .unwrap_or(false)
    }

    pub(crate) fn lookup(&self, realm: &Realm) -> Option<Binding> {
        self.store.read().ok()?.get(realm).cloned()
    }

    pub(crate) fn bind(&self, realm: Realm, binding: Binding) {
        if let Ok(mut store) = self.store.write() {
            store.insert(realm, binding);
        }
    }

    /// Move a binding from one realm to another, without disturbing its owner.
    pub(crate) fn rebind(&self, from: &Realm, to: Realm, binding: Binding) {
        if let Ok(mut store) = self.store.write() {
            store.remove(from);
            store.insert(to, binding);
        }
    }

    pub(crate) fn unbind(&self, realm: &Realm, key: &'static str) {
        if let Ok(mut store) = self.store.write() {
            store.remove(realm);
        }
        self.enqueue_dependents(key, realm);
    }

    pub(crate) fn enqueue(&self, uid: Uid) {
        if let Ok(mut queue) = self.queue.lock()
            && !queue.contains(&uid)
        {
            queue.push_back(uid);
        }
    }

    fn dequeue(&self) -> Option<Uid> {
        self.queue.lock().ok()?.pop_front()
    }

    /// Algorithm 3. A dependent is admitted only when its own realm for the key
    /// is the realm the binding sits in — declaring the key is not enough.
    fn enqueue_dependents(&self, key: &'static str, realm: &Realm) {
        let Ok(index) = self.by_key.lock() else {
            return;
        };
        let Some(candidates) = index.get(key) else {
            return;
        };
        let affected: Vec<Uid> = candidates
            .iter()
            .copied()
            .filter(|uid| {
                self.with_fiber(*uid, |fiber| fiber.realms.realm(key) == *realm)
                    .unwrap_or(false)
            })
            .collect();
        drop(index);
        for uid in affected {
            self.enqueue(uid);
        }
    }

    pub(crate) fn resolve_view(&self, uid: Uid, realms: &Realms) -> HashMap<&'static str, Binding> {
        let keys = self
            .with_fiber(uid, |fiber| fiber.inject.clone())
            .unwrap_or_default();
        let mut view = HashMap::with_capacity(keys.len());
        for key in keys {
            if let Some(binding) = self
                .lookup(&realms.realm(key))
                .filter(|binding| self.is_active(binding.provider))
            {
                view.insert(key, binding);
            }
        }
        view
    }

    /// target(γ, n): the provider of each declared key, or ⊥ when any is
    /// missing. A provider counts only while it is `ACTIVE`, which is what
    /// makes a withdrawal visible to dependents one step before it happens.
    fn resolve_target(&self, uid: Uid) -> Option<Target> {
        let (inject, realms, disabled) = self.with_fiber(uid, |fiber| {
            (
                fiber.inject.clone(),
                Arc::clone(&fiber.realms),
                fiber.disabled,
            )
        })?;
        if disabled {
            return None;
        }
        let mut providers = Vec::with_capacity(inject.len());
        for key in inject {
            let binding = self
                .lookup(&realms.realm(key))
                .filter(|binding| self.is_active(binding.provider))?;
            providers.push(binding.provider);
        }
        Some(Target(providers))
    }

    pub(crate) fn instantiate(
        &self,
        parent: Option<Uid>,
        component: Arc<dyn Component>,
        config: Value,
        realms: Arc<Realms>,
    ) -> Result<Uid, String> {
        validate(&component, &config)?;
        let uid = Uid::fresh();
        let fiber = Fiber::new(uid, parent, component, config, realms);
        let inject = fiber.inject.clone();
        self.refuse_cycle(&fiber)?;

        if let Ok(mut fibers) = self.fibers.lock() {
            fibers.insert(uid, fiber);
            if let Some(parent) = parent
                && let Some(entry) = fibers.get_mut(&parent)
            {
                entry.children.push(uid);
            }
        }
        if let Ok(mut index) = self.by_key.lock() {
            for key in inject {
                index.entry(key).or_default().push(uid);
            }
        }
        Ok(uid)
    }

    /// Two components that each inject a key the other provides can never both
    /// activate. That is predictable from the declarations alone, so it is
    /// reported here rather than left as two fibers quietly doing nothing.
    fn refuse_cycle(&self, candidate: &Fiber) -> Result<(), String> {
        let Ok(fibers) = self.fibers.lock() else {
            return Ok(());
        };
        for other in fibers.values() {
            let candidate_needs_other = candidate
                .inject
                .iter()
                .any(|key| other.provides.contains(key));
            let other_needs_candidate = other
                .inject
                .iter()
                .any(|key| candidate.provides.contains(key));
            if candidate_needs_other && other_needs_candidate {
                return Err(format!(
                    "`{}` and `{}` each require a service the other provides, \
                     so neither could ever load",
                    candidate.name, other.name
                ));
            }
        }
        Ok(())
    }

    fn context(&self, uid: Uid, inner: &Arc<Inner>) -> Option<Context> {
        let realms = self.with_fiber(uid, |fiber| Arc::clone(&fiber.realms))?;
        Some(Context {
            inner: Arc::clone(inner),
            fiber: uid,
            realms,
            pending: Arc::default(),
        })
    }

    /// O-Retire: force a child's target to ⊥ and unload it.
    ///
    /// Forcing means `disabled`, not merely clearing `target`: `refresh`
    /// recomputes the target from the store, so a child with nothing to inject
    /// would resolve straight back to satisfied and never come down.
    pub(crate) async fn retire(self: Arc<Self>, uid: Uid) {
        self.with_fiber_mut(uid, |fiber| fiber.disabled = true);
        Runtime { inner: self }.refresh(uid).await;
    }
}

fn validate(component: &Arc<dyn Component>, config: &Value) -> Result<(), String> {
    let Some(schema) = component.schema() else {
        return Ok(());
    };
    super::schema::validate(schema, config)
        .map_err(|error| format!("invalid config for `{}`: {error}", component.name()))
}

/// A running composition.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Arc<Inner>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new()
    }
}

impl Runtime {
    #[must_use]
    pub fn new() -> Runtime {
        Runtime {
            inner: Inner::new(),
        }
    }

    /// Mount a component at the root of the composition.
    ///
    /// # Errors
    ///
    /// When the configuration fails the component's schema, or when the
    /// component would close a dependency cycle.
    pub fn mount(&self, component: Arc<dyn Component>, config: Value) -> Result<Uid, String> {
        self.mount_in(component, config, Realms::root())
    }

    /// Mount at the root, with a realm table of the caller's choosing.
    ///
    /// # Errors
    ///
    /// As [`Runtime::mount`].
    pub fn mount_in(
        &self,
        component: Arc<dyn Component>,
        config: Value,
        realms: Arc<Realms>,
    ) -> Result<Uid, String> {
        let uid = self.inner.instantiate(None, component, config, realms)?;
        self.inner.enqueue(uid);
        Ok(uid)
    }

    /// Unload a fiber and everything it mounted.
    pub async fn unmount(&self, uid: Uid) {
        self.inner
            .with_fiber_mut(uid, |fiber| fiber.disabled = true);
        self.inner.enqueue(uid);
        self.settle().await;
    }

    /// Switch a fiber back on.
    pub async fn enable(&self, uid: Uid) {
        self.inner
            .with_fiber_mut(uid, |fiber| fiber.disabled = false);
        self.inner.enqueue(uid);
        self.settle().await;
    }

    /// Run every pending transition until nothing is left to do.
    ///
    /// The system quiesces: a fiber only ever waits on dependents that have
    /// already stopped being satisfiable, so the provider graph is walked on
    /// demand and never revisited in a loop.
    pub async fn settle(&self) {
        while let Some(uid) = self.inner.dequeue() {
            self.refresh(uid).await;
        }
    }

    /// Unload the whole composition, roots last.
    pub async fn shutdown(&self) {
        let roots: Vec<Uid> = self
            .inner
            .fibers
            .lock()
            .map(|fibers| {
                fibers
                    .values()
                    .filter(|fiber| fiber.parent.is_none())
                    .map(|fiber| fiber.uid)
                    .collect()
            })
            .unwrap_or_default();
        for uid in roots {
            self.inner
                .with_fiber_mut(uid, |fiber| fiber.disabled = true);
            self.inner.enqueue(uid);
        }
        self.settle().await;
    }

    /// Every fiber, for diagnostics.
    ///
    /// Two passes on purpose: the roster is read under the fiber lock and the
    /// bindings are resolved after it is dropped. Resolving inside would take
    /// the same lock again, and it is not reentrant.
    #[must_use]
    pub fn roster(&self) -> Vec<Status> {
        struct Row {
            status: Status,
            realms: Arc<Realms>,
        }

        let Ok(fibers) = self.inner.fibers.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<Row> = fibers
            .values()
            .map(|fiber| Row {
                status: Status {
                    uid: fiber.uid,
                    name: fiber.name.clone(),
                    state: fiber.state,
                    parent: fiber.parent,
                    inject: fiber.inject.clone(),
                    provides: fiber.provides.clone(),
                    missing: Vec::new(),
                    error: fiber.error.clone(),
                },
                realms: Arc::clone(&fiber.realms),
            })
            .collect();
        drop(fibers);

        for row in &mut rows {
            row.status.missing = row
                .status
                .inject
                .iter()
                .copied()
                .filter(|key| {
                    self.inner
                        .lookup(&row.realms.realm(key))
                        .filter(|binding| self.inner.is_active(binding.provider))
                        .is_none()
                })
                .collect();
        }

        let mut roster: Vec<Status> = rows.into_iter().map(|row| row.status).collect();
        roster.sort_by_key(|status| status.uid);
        roster
    }

    #[must_use]
    pub fn state(&self, uid: Uid) -> Option<State> {
        self.inner.with_fiber(uid, |fiber| fiber.state)
    }

    /// The keys currently bound, with the fiber that bound each. The state a
    /// quiescent composition amounts to, which is what the metatheory compares.
    #[must_use]
    pub fn bindings(&self) -> Vec<(Realm, Uid)> {
        let Ok(store) = self.inner.store.read() else {
            return Vec::new();
        };
        let mut bindings: Vec<(Realm, Uid)> = store
            .iter()
            .map(|(realm, binding)| (realm.clone(), binding.provider))
            .collect();
        bindings.sort_by(|a, b| a.0.key().cmp(b.0.key()).then(a.1.cmp(&b.1)));
        bindings
    }

    /// Move a fiber to a different set of realms.
    ///
    /// The hard part is not swapping the table, it is deciding what moves with
    /// it. A realm can be shared by several fibers of which only one is the
    /// provider, so "this binding is mine" is not a question the store can
    /// answer.
    ///
    /// Cordis answers it with delimiters: a tag written on a context and
    /// inherited by every context derived from it, so that two contexts agree
    /// exactly when they share an isolate scope. It needs them because a
    /// context there is a value in a prototype chain with no way back to a
    /// component. Here every context belongs to a fiber and every fiber knows
    /// its parent, so the same question — *is this context derived from the
    /// one being moved?* — is a walk up the tree. The tags would be a second,
    /// weaker way of asking it.
    pub async fn reassign(&self, uid: Uid, realms: Arc<Realms>) {
        let Some(previous) = self
            .inner
            .with_fiber(uid, |fiber| Arc::clone(&fiber.realms))
        else {
            return;
        };
        let changed = previous.divergence(&realms);
        if changed.is_empty() {
            return;
        }

        // Captured before anything moves: which realm each key came from, and
        // whether whoever provides it belongs to the scope being moved.
        let diff: Vec<Move> = changed
            .iter()
            .map(|key| {
                let from = previous.realm(key);
                let provider_is_ours = self
                    .inner
                    .lookup(&from)
                    .is_some_and(|binding| self.inner.descends_from(binding.provider, uid));
                Move {
                    key,
                    from,
                    to: realms.realm(key),
                    provider_is_ours,
                }
            })
            .collect();

        self.inner
            .with_fiber_mut(uid, |fiber| fiber.realms = Arc::clone(&realms));
        self.inner.enqueue(uid);
        self.settle().await;

        // A binding whose provider is inside the scope being moved is that
        // scope's own, so it travels; one that merely shared the realm stays.
        for step in &diff {
            if step.provider_is_ours
                && self.inner.lookup(&step.to).is_none()
                && let Some(binding) = self.inner.lookup(&step.from)
            {
                self.inner.rebind(&step.from, step.to.clone(), binding);
            }
        }

        for step in &diff {
            self.notify_across(uid, step);
        }
        self.settle().await;
    }

    /// Wake the dependents a move actually reached.
    ///
    /// Not everything that resolves the key: a fiber in neither realm is
    /// untouched, and a fiber on the same side of the scope boundary as the
    /// provider sees afterwards exactly what it saw before. The move only
    /// changes things for the fibers it *separates* from the provider, or joins
    /// to it.
    fn notify_across(&self, moved: Uid, step: &Move) {
        let Ok(index) = self.inner.by_key.lock() else {
            return;
        };
        let Some(candidates) = index.get(step.key).cloned() else {
            return;
        };
        drop(index);

        for candidate in candidates {
            let touched = self
                .inner
                .with_fiber(candidate, |fiber| {
                    let realm = fiber.realms.realm(step.key);
                    realm == step.from || realm == step.to
                })
                .unwrap_or(false)
                && self.inner.descends_from(candidate, moved) != step.provider_is_ours;
            if touched {
                self.inner.enqueue(candidate);
            }
        }
    }

    // ------------------------------------------------------------- transitions

    fn refresh(&self, uid: Uid) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let target = self.inner.resolve_target(uid);
            let go = self.inner.with_fiber_mut(uid, |fiber| {
                if target == fiber.target {
                    return None;
                }
                fiber.target = target.clone();
                if fiber.transitioning {
                    return None;
                }
                fiber.transitioning = true;
                if target.is_some() {
                    fiber.state = State::Loading;
                } else {
                    // Out of service before any inverse is scheduled: the
                    // dependents recompute against this while every binding
                    // this fiber installed is still standing.
                    fiber.state = State::Unloading;
                }
                Some(target.is_some())
            });

            match go.flatten() {
                Some(true) => self.load(uid).await,
                Some(false) => self.unload(uid).await,
                None => {}
            }
        })
    }

    fn load(&self, uid: Uid) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let target = self
                .inner
                .with_fiber(uid, |fiber| fiber.target.clone())
                .flatten();
            let Some(context) = self.inner.context(uid, &self.inner) else {
                return;
            };

            let view = context.view();
            self.inner
                .with_fiber_mut(uid, |fiber| fiber.committed = Some(view));

            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.inner
                    .with_fiber(uid, |fiber| Arc::clone(&fiber.component))
                    .ok_or_else(|| "fiber vanished mid-load".to_owned())
                    .and_then(|component| component.apply(&context))
            }))
            .unwrap_or_else(|payload| Err(panic_message(&payload)));

            let Pending {
                mut effects,
                acquiring,
                notify,
            } = context.take_pending();
            for acquire in acquiring {
                effects.push(Handle::new(acquire().await));
            }
            self.inner.with_fiber_mut(uid, |fiber| {
                fiber.dispose.append(&mut effects);
            });

            if let Err(error) = outcome {
                self.inner.with_fiber_mut(uid, |fiber| {
                    fiber.error = Some(error);
                    fiber.target = None;
                    fiber.state = State::Failed;
                    fiber.transitioning = false;
                });
                // Whatever it managed to register before raising still has to
                // come back off.
                self.unload_effects(uid).await;
                return;
            }

            let stale = self
                .inner
                .with_fiber(uid, |fiber| fiber.target != target)
                .unwrap_or(true);
            if stale {
                self.inner.with_fiber_mut(uid, |fiber| {
                    fiber.state = State::Unloading;
                });
                self.unload(uid).await;
                return;
            }

            self.inner.with_fiber_mut(uid, |fiber| {
                fiber.state = State::Active;
                fiber.error = None;
                fiber.transitioning = false;
            });
            self.announce(uid, &notify);
        })
    }

    fn unload(&self, uid: Uid) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let provides = self
                .inner
                .with_fiber(uid, |fiber| {
                    let mut keys = fiber.provides.clone();
                    keys.extend(fiber.realms.keys());
                    keys
                })
                .unwrap_or_default();

            // Drain the dependents first. Each is unsatisfiable already —
            // this fiber left service before any of this was scheduled — so
            // they tear down while its bindings still stand, and this waits
            // for them rather than pulling the floor out.
            let realms = self
                .inner
                .with_fiber(uid, |fiber| Arc::clone(&fiber.realms));
            if let Some(realms) = realms {
                for key in &provides {
                    self.inner.enqueue_dependents(key, &realms.realm(key));
                }
            }
            for dependent in self.drain_queue_excluding(uid) {
                self.refresh(dependent).await;
            }

            self.unload_effects(uid).await;

            let restart = self.inner.with_fiber_mut(uid, |fiber| {
                fiber.committed = None;
                if fiber.target.is_some() {
                    fiber.state = State::Loading;
                    true
                } else {
                    fiber.state = if fiber.error.is_some() {
                        State::Failed
                    } else if fiber.disabled {
                        State::Inactive
                    } else {
                        State::Pending
                    };
                    fiber.transitioning = false;
                    false
                }
            });
            if restart == Some(true) {
                self.load(uid).await;
            }
        })
    }

    async fn unload_effects(&self, uid: Uid) {
        let mut handles = self
            .inner
            .with_fiber_mut(uid, |fiber| std::mem::take(&mut fiber.dispose))
            .unwrap_or_default();
        unwind(&mut handles).await;
    }

    fn drain_queue_excluding(&self, uid: Uid) -> Vec<Uid> {
        let Ok(mut queue) = self.inner.queue.lock() else {
            return Vec::new();
        };
        let mut taken = Vec::new();
        let mut seen = HashSet::new();
        queue.retain(|candidate| {
            if *candidate == uid || !seen.insert(*candidate) {
                return true;
            }
            taken.push(*candidate);
            false
        });
        taken
    }

    fn announce(&self, uid: Uid, keys: &[&'static str]) {
        let Some(realms) = self
            .inner
            .with_fiber(uid, |fiber| Arc::clone(&fiber.realms))
        else {
            return;
        };
        for key in keys {
            self.inner.enqueue_dependents(key, &realms.realm(key));
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_owned())
}
