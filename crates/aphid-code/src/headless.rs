//! Running the harness without a terminal UI.
//!
//! Same agent, same tools, same plugins — the only difference is that the
//! reporting plugin writes to stdout instead of driving a ratatui app. That
//! makes this the mode to reach for in scripts, in CI, and when checking whether
//! a problem is in the harness or in the UI.

use std::io::Write;
use std::sync::Mutex;

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, RunOutcome, StreamCx, ToolOutcome,
    TurnSummary,
};
use aphid_core::{BlockKind, Event};

use aphid_core::Transcript;

use crate::harness::{self, Harness, HarnessOptions};
use crate::session;

/// Prints a run to stdout as it happens.
///
/// Hooks are synchronous and can be called from a tool's own task, so the
/// line-tracking state sits behind a mutex. There is nothing to contend for: the
/// lock is held only for the length of one write.
pub struct Printer {
    state: Mutex<State>,
    quiet: bool,
}

#[derive(Default)]
struct State {
    /// Set while the cursor is mid-line, so the next thing printed starts fresh.
    line_open: bool,
}

impl Printer {
    /// `quiet` drops the per-line output of running tools, keeping one line per
    /// call and result.
    #[must_use]
    pub fn new(quiet: bool) -> Self {
        Self {
            state: Mutex::new(State::default()),
            quiet,
        }
    }

    fn write(&self, text: &str, keep_open: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let mut out = std::io::stdout().lock();
        if state.line_open && !keep_open {
            let _ = writeln!(out);
            state.line_open = false;
        }
        let _ = write!(out, "{text}");
        state.line_open = keep_open && !text.ends_with('\n');
        let _ = out.flush();
    }

    fn line(&self, text: &str) {
        self.write(&format!("{text}\n"), false);
    }
}

impl Plugin for Printer {
    fn name(&self) -> &str {
        "stdout-printer"
    }

    fn interests(&self) -> Interest {
        let base = Interest::EVENT
            | Interest::TOOL_CALL
            | Interest::TOOL_RESULT
            | Interest::TURN_END
            | Interest::RUN_END;
        if self.quiet {
            base
        } else {
            base | Interest::TOOL_PROGRESS
        }
    }

    fn on_event(&self, event: &Event, cx: &StreamCx<'_>) {
        // Only assistant prose goes to stdout as it streams. Tool-call arguments
        // arrive as deltas too, and printing raw JSON token by token helps
        // nobody — the call is announced in full by `on_tool_call`.
        if let Event::Delta {
            kind: BlockKind::Text,
            span,
            ..
        } = *event
        {
            self.write(cx.text(span), true);
        }
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        self.line(&format!("→ {} {}", call.name(), call.arguments()));
        Guard::Allow
    }

    fn on_tool_progress(&self, _call_id: &str, _tool: &str, chunk: &str) {
        self.line(&format!("  │ {chunk}"));
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        let body = outcome.text_content();
        let summary = body.lines().next().unwrap_or("").trim();
        let arrow = if outcome.is_error { "✗" } else { "←" };
        let more = match body.lines().count() {
            0 | 1 => String::new(),
            n => format!("  (+{} lines)", n - 1),
        };
        self.line(&format!("{arrow} {} {summary}{more}", cx.name()));
    }

    fn on_turn_end(&self, _cx: &mut Cx<'_>, _turn: &TurnSummary) -> Flow {
        self.write("", false);
        Flow::Continue
    }

    fn on_run_end(&self, _cx: &mut Cx<'_>, outcome: &RunOutcome) {
        self.write("", false);
        let usage = outcome.usage;
        self.line(&format!(
            "\n{} turns · in {} · cached {} · out {} · ${:.6}",
            outcome.turns, usage.input, usage.cache_read, usage.output, usage.cost.total
        ));
        if let Some(error) = &outcome.error {
            self.line(&format!("error: {error}"));
        }
    }
}

/// Build a harness that reports to stdout and run one prompt through it.
///
/// `resumed` is a conversation to continue, as produced by
/// [`session::attach`](crate::session::attach).
///
/// Returns the harness too, so a caller can inspect the transcript or keep
/// prompting.
pub async fn run(
    mut options: HarnessOptions,
    prompt: &str,
    quiet: bool,
    resumed: Option<Transcript>,
) -> (Harness, RunOutcome) {
    options
        .plugins
        .push(std::sync::Arc::new(Printer::new(quiet)));

    let mut harness = harness::build(options);
    if let Some(transcript) = resumed {
        let restored = session::splice(&mut harness.agent, &transcript);
        eprintln!("aphid: resumed {restored} messages");
    }
    for note in &harness.notes {
        eprintln!("aphid: {note}");
    }
    for diagnostic in &harness.diagnostics {
        eprintln!(
            "aphid: skipped skill {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        );
    }

    let outcome = harness.agent.prompt(prompt).await;
    (harness, outcome)
}
