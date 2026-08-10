//! Persisting a session as it happens.

use std::sync::Mutex;

use aphid_agent::{Cx, Flow, Interest, Plugin, RunOutcome, TurnSummary};

use super::store::SessionStore;

/// Appends new messages to a session file at every point the transcript grows.
///
/// A plugin rather than something the app drives, because the hooks see the
/// transcript at exactly the moments it changes — after the prompt is appended,
/// and after each turn's results are committed. A crash therefore costs at most
/// the turn that was in flight.
pub struct SessionPlugin {
    store: Mutex<SessionStore>,
}

impl SessionPlugin {
    #[must_use]
    pub fn new(store: SessionStore) -> Self {
        Self {
            store: Mutex::new(store),
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

    /// Write whatever is new. A write failure is reported once and then ignored:
    /// losing the log is not a reason to lose the conversation.
    fn flush(&self, cx: &Cx<'_>) {
        if let Ok(mut store) = self.store.lock()
            && let Err(error) = store.flush(cx.transcript())
        {
            eprintln!("aphid: could not write the session: {error}");
        }
    }
}

impl Plugin for SessionPlugin {
    fn name(&self) -> &str {
        "session"
    }

    fn interests(&self) -> Interest {
        Interest::RUN_START | Interest::TURN_END | Interest::RUN_END
    }

    fn on_run_start(&self, cx: &mut Cx<'_>) {
        self.flush(cx);
    }

    fn on_turn_end(&self, cx: &mut Cx<'_>, _turn: &TurnSummary) -> Flow {
        self.flush(cx);
        Flow::Continue
    }

    fn on_run_end(&self, cx: &mut Cx<'_>, _outcome: &RunOutcome) {
        self.flush(cx);
    }
}
