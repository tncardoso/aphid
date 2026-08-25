//! The run, as a script sees it.
//!
//! Rhai passes arguments to script functions **by value**, so a script cannot
//! mutate a payload map and have the host see it. Payloads therefore travel as
//! maps and edits come back as return values.
//!
//! The context is the exception. [`Run`] holds a handle rather than a copy —
//! what a listener asks for is recorded and applied by the loop once dispatch
//! returns — so `cx.note(…)` works whatever Rhai does with the value on the
//! way in, and works from wherever the listener happens to run.

use aphid_agent::Run;
use rhai::Engine;

/// The run, as a script sees it.
///
/// A thin wrapper: the deferral, the sharing and the application all belong to
/// [`Run`], and duplicating them here was the previous version's only job.
#[derive(Clone)]
pub struct ScriptCx {
    run: Run,
}

impl ScriptCx {
    pub(crate) fn new(run: &Run) -> Self {
        Self { run: run.clone() }
    }

    pub(crate) fn note(&mut self, text: &str) {
        self.run.note(text);
    }

    pub(crate) fn push_user(&mut self, text: &str) {
        self.run.push_user(text);
    }

    pub(crate) fn cancel(&mut self) {
        self.run.cancel();
    }

    pub(crate) fn model(&mut self) -> String {
        self.run.model.id.to_string()
    }

    pub(crate) fn turn(&mut self) -> i64 {
        i64::from(self.run.turn)
    }

    pub(crate) fn input_tokens(&mut self) -> i64 {
        i64::from(self.run.usage.input)
    }

    pub(crate) fn output_tokens(&mut self) -> i64 {
        i64::from(self.run.usage.output)
    }
}

/// Teach an engine about `Cx`.
///
/// The getters are snapshots and the methods are requests: a script reads
/// `cx.turn` and calls `cx.note(…)`, and the loop applies the requests once
/// the announcement it came from has finished going round.
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
            .field("model", &self.run.model.id)
            .field("turn", &self.run.turn)
            .finish()
    }
}
