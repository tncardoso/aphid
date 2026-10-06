//! Persisting a session as it happens.

use std::sync::{Arc, Mutex};

use aphid_agent::TranscriptListeners;
use aphid_agent::rt::{Component, Context, Disposer, Scope};

use super::store::SessionStore;

/// Appends new messages to a session file at every moment the transcript grows.
///
/// A component rather than something the app drives, because the moments it
/// listens for are exactly the moments the transcript changes — after the
/// prompt is appended, after each turn's results are committed, and at the end
/// of the run. A crash therefore costs at most the turn that was in flight.
pub struct SessionComponent {
    store: Arc<Mutex<SessionStore>>,
    listeners: Arc<TranscriptListeners>,
    /// The conversation this component writes for. Set by an alate after the
    /// session file has been opened (the file *is* the id); left `None` by a
    /// standalone agent, which has one conversation and hears everything.
    scope: Mutex<Scope>,
}

impl SessionComponent {
    #[must_use]
    pub fn new(store: SessionStore, listeners: Arc<TranscriptListeners>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            listeners,
            scope: Mutex::new(None),
        }
    }

    /// The session file's path, for `/session` and for reporting at startup.
    #[must_use]
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.store
            .lock()
            .ok()
            .map(|store| store.path().to_path_buf())
    }

    #[must_use]
    pub fn id(&self) -> Option<String> {
        self.store.lock().ok().map(|store| store.id().to_owned())
    }

    /// Run `f` on the store, the file this component writes.
    ///
    /// # Errors
    ///
    /// Fails when the lock was poisoned by a panic in another writer.
    pub fn with_store<T>(&self, f: impl FnOnce(&mut SessionStore) -> T) -> Result<T, String> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| "the session store is poisoned".to_owned())?;
        Ok(f(&mut store))
    }

    /// Write to another session file from now on. Returns the one written
    /// before.
    ///
    /// # Errors
    ///
    /// Fails when the lock was poisoned.
    pub fn switch(&self, store: SessionStore) -> Result<SessionStore, String> {
        self.with_store(|current| std::mem::replace(current, store))
    }

    /// Which session this component writes for, so an alate that hosts several
    /// keeps each transcript to its own conversation. Must be called before the
    /// component is mounted; `None` (the default) hears every transcript.
    pub fn set_scope(&self, scope: Scope) {
        if let Ok(mut slot) = self.scope.lock() {
            *slot = scope;
        }
    }
}

impl Component for SessionComponent {
    fn name(&self) -> &str {
        "session"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let store = Arc::clone(&self.store);
        let listeners = Arc::clone(&self.listeners);
        let owner = ctx.uid();
        let scope = self.scope.lock().ok().and_then(|scope| scope.clone());

        listeners.subscribe_scoped(scope, owner, move |moment, transcript, _run| {
            // Every moment this hears about is one where the transcript grew,
            // so there is nothing to filter on.
            let _ = moment;
            // A write failure is reported once and then ignored: losing the log
            // is not a reason to lose the conversation.
            if let Ok(mut store) = store.lock()
                && let Err(error) = store.flush(transcript)
            {
                eprintln!("aphid: could not write the session: {error}");
            }
        });

        let listeners = Arc::clone(&self.listeners);
        ctx.effect(move || Disposer::sync(move || listeners.unsubscribe(owner)));
        Ok(())
    }
}
