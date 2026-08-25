//! Revertible effects: every context mutation carries its inverse.
//!
//! Every mutation a component makes through the context flows through
//! [`Context::effect`](super::Context::effect), so nothing has to be
//! remembered by hand and nothing has to be unregistered by hand. The body
//! runs at load and returns a [`Disposer`]; the runtime runs the disposer at
//! unload.
//!
//! What the runtime does **not** check is that the disposer actually reverses
//! the body. That is an obligation on the component author, and where it
//! cannot be met — bytes already on a socket, a message already sent — the
//! honest answer is compensation rather than reversal. See the system boundary
//! in the module documentation of [`rt`](super).
//!
//! # Ordering
//!
//! Disposers run in reverse registration order, and each is **awaited before
//! the next begins**. Cordis runs async disposers concurrently and warns
//! authors to fold sequential teardown into one disposer; running them in
//! sequence is stronger, needs no join combinator, and removes the footgun.
//! The cost is that a slow disposer delays the ones under it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tool::BoxFuture;

/// The inverse of an effect. Fires at most once.
pub enum Disposer {
    /// Nothing to undo. The honest answer for a purely observational effect.
    Nop,
    Sync(Box<dyn FnOnce() + Send>),
    /// An inverse that has to await — closing a connection, stopping a
    /// process, flushing a file.
    Later(Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>),
}

impl Disposer {
    #[must_use]
    pub fn sync(f: impl FnOnce() + Send + 'static) -> Self {
        Disposer::Sync(Box::new(f))
    }

    /// An inverse that awaits.
    ///
    /// The closure is `Send` so it can be built anywhere; the future it returns
    /// is `Send` because the runtime may be driven from any thread.
    #[must_use]
    pub fn later<F>(f: impl FnOnce() -> F + Send + 'static) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Disposer::Later(Box::new(move || Box::pin(f())))
    }

    async fn run(self) {
        match self {
            Disposer::Nop => {}
            Disposer::Sync(f) => f(),
            Disposer::Later(f) => f().await,
        }
    }
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Disposer::Nop => "Disposer::Nop",
            Disposer::Sync(_) => "Disposer::Sync",
            Disposer::Later(_) => "Disposer::Later",
        })
    }
}

struct Slot {
    /// Whether the inverse is still owed. Flipping it to `false` is what makes
    /// recovery fire at most once: firing twice would apply an inverse at a
    /// state no application of the effect produced, where nothing holds it to
    /// reverting anything.
    armed: AtomicBool,
    disposer: Mutex<Disposer>,
}

/// A registered effect, and the right to revert it early.
///
/// Holding one is optional: the fiber that registered the effect owns a copy
/// and will revert it on unload whether or not the caller keeps this.
#[derive(Clone)]
pub struct Handle {
    slot: Arc<Slot>,
}

impl Handle {
    pub(crate) fn new(disposer: Disposer) -> Handle {
        Handle {
            slot: Arc::new(Slot {
                armed: AtomicBool::new(true),
                disposer: Mutex::new(disposer),
            }),
        }
    }

    /// A handle over an effect that has already been reverted, or never had
    /// anything to revert.
    #[must_use]
    pub fn inert() -> Handle {
        let handle = Handle::new(Disposer::Nop);
        handle.slot.armed.store(false, Ordering::Release);
        handle
    }

    /// Revert the effect now. Subsequent calls do nothing.
    pub async fn dispose(&self) {
        if !self.slot.armed.swap(false, Ordering::AcqRel) {
            return;
        }
        // Taking the disposer out under the lock keeps the lock off the await.
        let disposer = match self.slot.disposer.lock() {
            Ok(mut guard) => std::mem::replace(&mut *guard, Disposer::Nop),
            // A poisoned lock means a disposer panicked. There is nothing left
            // to run and nothing useful to say here; the panic was already
            // reported where it happened.
            Err(_) => Disposer::Nop,
        };
        disposer.run().await;
    }

    /// Whether the inverse is still owed.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.slot.armed.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("armed", &self.is_armed())
            .finish()
    }
}

/// Revert a fiber's effects: reverse order, one at a time.
pub(crate) async fn unwind(handles: &mut Vec<Handle>) {
    while let Some(handle) = handles.pop() {
        handle.dispose().await;
    }
}
