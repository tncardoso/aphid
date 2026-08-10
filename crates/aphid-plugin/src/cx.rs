//! The context objects a hook is handed.
//!
//! Rhai passes arguments to script functions **by value**, so a script cannot
//! mutate a payload map and have the host see it. Payloads therefore travel as
//! maps and edits come back as return values.
//!
//! Contexts are the exception. `Cx` is a registered type holding an `Arc` to a
//! list of actions, so `cx.note("…")` records something the host applies after
//! the call returns, however many times the value was cloned on the way in. That
//! indirection is also what makes an agent context — which borrows the
//! transcript mutably — expressible as a `'static` script value at all.

use std::sync::{Arc, Mutex};

use aphid_agent::Cx as AgentCx;
use rhai::Engine;

/// Something a script asked the host to do to the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Append a system message at the tail of the transcript.
    Note(String),
    /// Append a user message at the tail of the transcript.
    User(String),
    /// Ask the run to stop at the next checkpoint.
    Cancel,
}

/// The run, as a script sees it.
#[derive(Clone)]
pub struct ScriptCx {
    actions: Arc<Mutex<Vec<Action>>>,
    model: String,
    turn: i64,
    input: i64,
    output: i64,
}

impl ScriptCx {
    /// Snapshot an agent context. The scalars are copied because a script may
    /// hold the value past the hook; the actions are shared because that is the
    /// whole point.
    pub(crate) fn new(cx: &AgentCx<'_>) -> Self {
        let usage = cx.usage();
        Self {
            actions: Arc::new(Mutex::new(Vec::new())),
            model: cx.model().id.to_string(),
            turn: i64::from(cx.turn()),
            input: i64::from(usage.input),
            output: i64::from(usage.output),
        }
    }

    /// Apply everything the script asked for, in the order it asked.
    pub(crate) fn apply(&self, cx: &mut AgentCx<'_>) {
        let Ok(mut actions) = self.actions.lock() else {
            return;
        };
        for action in actions.drain(..) {
            match action {
                Action::Note(text) => {
                    cx.push_system_note(&text);
                }
                Action::User(text) => {
                    cx.push_user(&text);
                }
                Action::Cancel => cx.cancel(),
            }
        }
    }

    fn record(&mut self, action: Action) {
        if let Ok(mut actions) = self.actions.lock() {
            actions.push(action);
        }
    }

    pub(crate) fn note(&mut self, text: &str) {
        self.record(Action::Note(text.to_owned()));
    }

    pub(crate) fn push_user(&mut self, text: &str) {
        self.record(Action::User(text.to_owned()));
    }

    pub(crate) fn cancel(&mut self) {
        self.record(Action::Cancel);
    }

    pub(crate) fn model(&mut self) -> String {
        self.model.clone()
    }

    pub(crate) fn turn(&mut self) -> i64 {
        self.turn
    }

    pub(crate) fn input_tokens(&mut self) -> i64 {
        self.input
    }

    pub(crate) fn output_tokens(&mut self) -> i64 {
        self.output
    }
}

/// Teach an engine about `Cx`.
///
/// The getters are snapshots and the methods are requests: a script reads
/// `cx.turn` and calls `cx.note(…)`, and the host applies the requests once the
/// hook returns.
pub(crate) fn register(engine: &mut Engine) {
    engine
        .register_type_with_name::<ScriptCx>("Cx")
        .register_fn("note", ScriptCx::note)
        .register_fn("push_user", ScriptCx::push_user)
        .register_fn("cancel", ScriptCx::cancel)
        .register_get("model", ScriptCx::model)
        .register_get("turn", ScriptCx::turn)
        .register_get("input_tokens", ScriptCx::input_tokens)
        .register_get("output_tokens", ScriptCx::output_tokens)
        .register_fn("to_string", |cx: &mut ScriptCx| format!("{cx:?}"))
        .register_fn("to_debug", |cx: &mut ScriptCx| format!("{cx:?}"));
}

impl std::fmt::Debug for ScriptCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cx")
            .field("model", &self.model)
            .field("turn", &self.turn)
            .finish()
    }
}
