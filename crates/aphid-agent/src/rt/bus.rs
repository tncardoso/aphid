//! Events: talking without knowing who listens.
//!
//! A service is a direct call to something you named. An event is an
//! announcement to whoever cares, and it is what lets two components cooperate
//! without either importing the other.
//!
//! # The mode is part of the contract
//!
//! Which dispatch an event uses is not a runtime choice, it is a property of
//! the event: it decides whether listeners can answer, run concurrently, or
//! cut each other off. So it is a bound on the event type, not a flag.
//!
//! | Mode | Shape | What it means |
//! |---|---|---|
//! | [`Emitted`] | `emit(&mut E)` | Broadcast. Every listener runs and may edit the payload in place. |
//! | [`Paralleled`] | `parallel(E)` | Every listener runs concurrently and all are awaited. |
//! | [`Serialed`] | `serial(E)` | Listeners run in order, awaited; the first answer wins. |
//! | [`Bailed`] | `bail(&E)` | The synchronous form of serial. |
//! | [`Waterfalled`] | `waterfall(input, tail)` | Around-middleware: transform, or answer instead. |
//!
//! # Failure
//!
//! Whether a listener that raises is ignored or is treated as a refusal is
//! also part of the event's contract, through [`Event::FAILURE`]. Most events
//! are [`Failure::Open`]: a broken listener should not take the session with
//! it. The ones people write in order to be safe — a guard on a tool call, a
//! permission decision — are [`Failure::Closed`], because a guard that raised
//! has not agreed to anything, and carrying on as if it had would defeat the
//! only reason it exists.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use super::uid::Uid;
use crate::tool::BoxFuture;

/// A listener's reach, and the tag a run stamps its announcements with.
///
/// `None` hears everything — the daemon's own announcements and every
/// session's — which is what a component that is daemon-wide by design wants.
/// `Some(s)` hears only the announcements a run stamped with the same `s`,
/// which is what a per-session component wants: an alate hosts several agents,
/// and one conversation must not see another's.
pub type Scope = Option<Arc<str>>;

/// What happens when a listener raises.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    /// Report it and carry on. A broken listener is not the session's problem.
    Open,
    /// Treat it as a refusal. For the events people write in order to be safe.
    Closed,
}

/// Something components announce.
pub trait Event: Send + Sync + 'static {
    /// `namespace/action`, which keeps a flat namespace readable. Used in
    /// diagnostics, in the declaration check, and by a script bridge.
    const NAME: &'static str;

    /// What a raising listener means for this event.
    const FAILURE: Failure = Failure::Open;

    /// Whether this event may be announced from inside its own announcement.
    ///
    /// Almost every event may. The exception is an event a listener naturally
    /// causes: a listener for "something was shown to the user" that shows the
    /// user something would announce itself, forever. For those, the inner
    /// announcement is dropped.
    const REENTRANT: bool = true;
}

/// Broadcast. Listeners may edit the payload, and each sees the last one's edits.
pub trait Emitted: Event {}

/// Every listener runs concurrently, and all are awaited.
pub trait Paralleled: Event {}

/// Listeners run in order and the first answer stops the rest.
pub trait Serialed: Event {
    type Out: Send + 'static;
}

/// The synchronous form of [`Serialed`].
pub trait Bailed: Event {
    type Out: Send + 'static;
}

/// Around-middleware: a listener may transform what the rest returns, or
/// answer without calling them at all.
pub trait Waterfalled: Event {
    type In: Send + 'static;
    type Out: Send + 'static;
}

// ------------------------------------------------------------------ listeners

type EmitFn<E> = Arc<dyn Fn(&mut E) + Send + Sync>;
type ParallelFn<E> = Arc<dyn Fn(Arc<E>) -> BoxFuture<'static, ()> + Send + Sync>;
type SerialFn<E> =
    Arc<dyn Fn(Arc<E>) -> BoxFuture<'static, Option<<E as Serialed>::Out>> + Send + Sync>;
type BailFn<E> = Arc<dyn Fn(&E) -> Option<<E as Bailed>::Out> + Send + Sync>;

/// The rest of a waterfall chain, plus the default underneath it.
///
/// Taken by value and `#[must_use]` on purpose. Not calling it is a deliberate
/// veto of everything downstream, and it should look like one at the call site
/// — a listener that only observes and forgets to call it silently swallows
/// the default behaviour for everybody.
#[must_use = "not running the rest of the chain vetoes it — say so on purpose"]
pub struct Next<'a, E: Waterfalled> {
    rest: &'a [Arc<dyn WaterfallFn<E>>],
    tail: &'a (dyn Fn(E::In) -> E::Out + Sync),
}

impl<'a, E: Waterfalled> Next<'a, E> {
    /// Run the rest of the chain.
    pub fn run(self, input: E::In) -> E::Out {
        match self.rest.split_first() {
            Some((head, rest)) => head.call(
                input,
                Next {
                    rest,
                    tail: self.tail,
                },
            ),
            None => (self.tail)(input),
        }
    }
}

/// One link of a waterfall.
pub trait WaterfallFn<E: Waterfalled>: Send + Sync {
    fn call(&self, input: E::In, next: Next<'_, E>) -> E::Out;
}

impl<E, F> WaterfallFn<E> for F
where
    E: Waterfalled,
    F: for<'a> Fn(E::In, Next<'a, E>) -> E::Out + Send + Sync,
{
    fn call(&self, input: E::In, next: Next<'_, E>) -> E::Out {
        self(input, next)
    }
}

// ---------------------------------------------------------------------- slots

struct Slot<L> {
    /// The uid is what makes unsubscribing on unload a retain rather than
    /// bookkeeping the listener has to do itself. The scope is the session the
    /// listener belongs to, or `None` for a listener that wants everything.
    listeners: Vec<(Uid, Scope, L)>,
    /// Set while this event is being dispatched, for the events that cannot
    /// stand being announced from inside their own announcement.
    ///
    /// Atomic rather than a `Cell` because the bus is shared across threads —
    /// a listener runs wherever the announcement was made.
    dispatching: AtomicBool,
}

impl<L> Default for Slot<L> {
    fn default() -> Self {
        Slot {
            listeners: Vec::new(),
            dispatching: AtomicBool::new(false),
        }
    }
}

/// Where listeners live.
///
/// One hash lookup and one type check per dispatch, then a walk of a slice.
/// Registration is an effect, so a listener leaves with the component that
/// registered it and nothing has to be removed by hand.
#[derive(Default)]
pub struct Bus {
    slots: RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
    /// Every event name some mounted component declared it emits. Subscribing
    /// to a name nobody emits is a typo, and a typo that produces silence is
    /// the worst thing this model can hand somebody.
    declared: RwLock<HashMap<&'static str, usize>>,
}

impl Bus {
    #[must_use]
    pub fn new() -> Bus {
        Bus::default()
    }

    /// Record that a component emits these names, and forget again when it goes.
    pub fn declare(&self, names: &[&'static str]) {
        if let Ok(mut declared) = self.declared.write() {
            for name in names {
                *declared.entry(name).or_insert(0) += 1;
            }
        }
    }

    pub fn undeclare(&self, names: &[&'static str]) {
        if let Ok(mut declared) = self.declared.write() {
            for name in names {
                if let Some(count) = declared.get_mut(name) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        declared.remove(name);
                    }
                }
            }
        }
    }

    /// Whether anything mounted announces this name.
    #[must_use]
    pub fn is_declared(&self, name: &str) -> bool {
        self.declared
            .read()
            .map(|declared| declared.contains_key(name))
            .unwrap_or(false)
    }

    /// Whether anybody is listening. One lookup, and worth doing before
    /// building a payload nobody will read.
    #[must_use]
    pub fn has_listeners<E: Event>(&self) -> bool {
        self.slots
            .read()
            .map(|slots| slots.contains_key(&TypeId::of::<E>()))
            .unwrap_or(false)
    }

    fn insert<E: Event, L: Send + Sync + 'static>(&self, owner: Uid, scope: Scope, listener: L) {
        if let Ok(mut slots) = self.slots.write() {
            slots
                .entry(TypeId::of::<E>())
                .or_insert_with(|| Box::new(Slot::<L>::default()))
                .downcast_mut::<Slot<L>>()
                .expect("one listener shape per event, fixed by its mode")
                .listeners
                .push((owner, scope, listener));
        }
    }

    fn remove<E: Event, L: Send + Sync + 'static>(&self, owner: Uid) {
        if let Ok(mut slots) = self.slots.write()
            && let Some(slot) = slots
                .get_mut(&TypeId::of::<E>())
                .and_then(|slot| slot.downcast_mut::<Slot<L>>())
        {
            slot.listeners.retain(|(uid, _, _)| *uid != owner);
        }
    }

    /// The listeners an announcement reaches: the global ones, and the ones
    /// whose scope matches the announcement's.
    fn snapshot<E: Event, L: Clone + Send + Sync + 'static>(&self, scope: &Scope) -> Vec<L> {
        self.slots
            .read()
            .ok()
            .and_then(|slots| {
                slots
                    .get(&TypeId::of::<E>())
                    .and_then(|slot| slot.downcast_ref::<Slot<L>>())
                    .map(|slot| {
                        slot.listeners
                            .iter()
                            .filter(|(_, listener_scope, _)| {
                                listener_scope.is_none()
                                    || listener_scope.as_deref() == scope.as_deref()
                            })
                            .map(|(_, _, listener)| listener.clone())
                            .collect()
                    })
            })
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- subscribing

    /// Listen to every announcement, whatever session made it.
    pub fn on<E: Emitted>(&self, owner: Uid, listener: impl Fn(&mut E) + Send + Sync + 'static) {
        self.on_scoped(None, owner, listener);
    }

    /// Listen to one session's announcements only. See [`Scope`].
    pub fn on_scoped<E: Emitted>(
        &self,
        scope: Scope,
        owner: Uid,
        listener: impl Fn(&mut E) + Send + Sync + 'static,
    ) {
        self.insert::<E, EmitFn<E>>(owner, scope, Arc::new(listener));
    }

    pub fn on_parallel<E, F, Fut>(&self, owner: Uid, listener: F)
    where
        E: Paralleled,
        F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener: ParallelFn<E> = Arc::new(move |event| Box::pin(listener(event)));
        self.insert::<E, ParallelFn<E>>(owner, None, listener);
    }

    pub fn on_serial<E, F, Fut>(&self, owner: Uid, listener: F)
    where
        E: Serialed,
        F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<E::Out>> + Send + 'static,
    {
        let listener: SerialFn<E> = Arc::new(move |event| Box::pin(listener(event)));
        self.insert::<E, SerialFn<E>>(owner, None, listener);
    }

    pub fn on_bail<E: Bailed>(
        &self,
        owner: Uid,
        listener: impl Fn(&E) -> Option<E::Out> + Send + Sync + 'static,
    ) {
        self.insert::<E, BailFn<E>>(owner, None, Arc::new(listener));
    }

    pub fn on_waterfall<E: Waterfalled>(
        &self,
        owner: Uid,
        listener: impl WaterfallFn<E> + 'static,
    ) {
        self.insert::<E, Arc<dyn WaterfallFn<E>>>(owner, None, Arc::new(listener));
    }

    /// Drop every listener a fiber registered, whatever their modes.
    ///
    /// Called by the disposer the subscription installed, so this is the
    /// unwind path rather than something a component asks for.
    pub fn unsubscribe<E: Emitted>(&self, owner: Uid) {
        self.remove::<E, EmitFn<E>>(owner);
    }

    pub fn unsubscribe_bail<E: Bailed>(&self, owner: Uid) {
        self.remove::<E, BailFn<E>>(owner);
    }

    pub fn unsubscribe_parallel<E: Paralleled>(&self, owner: Uid) {
        self.remove::<E, ParallelFn<E>>(owner);
    }

    pub fn unsubscribe_serial<E: Serialed>(&self, owner: Uid) {
        self.remove::<E, SerialFn<E>>(owner);
    }

    pub fn unsubscribe_waterfall<E: Waterfalled>(&self, owner: Uid) {
        self.remove::<E, Arc<dyn WaterfallFn<E>>>(owner);
    }

    // -------------------------------------------------------------- dispatch

    /// Broadcast to every listener, whatever session made the announcement.
    pub fn emit<E: Emitted>(&self, event: &mut E) {
        self.emit_scoped(&None, event);
    }

    /// Broadcast to a scope. See [`Scope`].
    pub fn emit_scoped<E: Emitted>(&self, scope: &Scope, event: &mut E) {
        if !E::REENTRANT && !self.enter::<E, EmitFn<E>>() {
            return;
        }
        for listener in self.snapshot::<E, EmitFn<E>>(scope) {
            listener(event);
        }
        if !E::REENTRANT {
            self.leave::<E, EmitFn<E>>();
        }
    }

    /// Claim the dispatch, or report that somebody already has it.
    fn enter<E: Event, L: Send + Sync + 'static>(&self) -> bool {
        self.slots
            .read()
            .ok()
            .and_then(|slots| {
                slots
                    .get(&TypeId::of::<E>())
                    .and_then(|slot| slot.downcast_ref::<Slot<L>>())
                    .map(|slot| !slot.dispatching.swap(true, Ordering::AcqRel))
            })
            .unwrap_or(true)
    }

    fn leave<E: Event, L: Send + Sync + 'static>(&self) {
        if let Ok(slots) = self.slots.read()
            && let Some(slot) = slots
                .get(&TypeId::of::<E>())
                .and_then(|slot| slot.downcast_ref::<Slot<L>>())
        {
            slot.dispatching.store(false, Ordering::Release);
        }
    }

    /// Every listener at once, all awaited.
    pub async fn parallel<E: Paralleled>(&self, event: E) {
        let event = Arc::new(event);
        let listeners = self.snapshot::<E, ParallelFn<E>>(&None);
        // Started together rather than one after another: that is the whole
        // difference between this and `serial`.
        let running: Vec<_> = listeners
            .into_iter()
            .map(|listener| listener(Arc::clone(&event)))
            .collect();
        for task in running {
            task.await;
        }
    }

    /// In order, awaited, first answer wins.
    pub async fn serial<E: Serialed>(&self, event: E) -> Option<E::Out> {
        let event = Arc::new(event);
        for listener in self.snapshot::<E, SerialFn<E>>(&None) {
            if let Some(answer) = listener(Arc::clone(&event)).await {
                return Some(answer);
            }
        }
        None
    }

    /// The synchronous form of [`Bus::serial`].
    #[must_use]
    pub fn bail<E: Bailed>(&self, event: &E) -> Option<E::Out> {
        for listener in self.snapshot::<E, BailFn<E>>(&None) {
            if let Some(answer) = listener(event) {
                return Some(answer);
            }
        }
        None
    }

    /// Run the chain over `input`, ending in `tail` if nobody short-circuits.
    pub fn waterfall<E: Waterfalled>(
        &self,
        input: E::In,
        tail: &(dyn Fn(E::In) -> E::Out + Sync),
    ) -> E::Out {
        let listeners = self.snapshot::<E, Arc<dyn WaterfallFn<E>>>(&None);
        Next::<E> {
            rest: &listeners,
            tail,
        }
        .run(input)
    }
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.slots.read().map(|slots| slots.len()).unwrap_or(0);
        f.debug_struct("Bus").field("events", &count).finish()
    }
}
