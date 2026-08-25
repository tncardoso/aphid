//! Running the harness without a terminal UI.
//!
//! Same agent, same tools, same plugins — the only difference is that the
//! reporting plugin writes to stdout instead of driving a ratatui app. That
//! makes this the mode to reach for in scripts, in CI, and when checking whether
//! a problem is in the harness or in the UI.

use std::io::Write;
use std::sync::{Arc, Mutex};

use aphid_agent::rt::{Component, Composition, Context, Disposer};
use aphid_agent::{
    RunEnd, RunOutcome, ToolContent, ToolProgress, ToolRequest, ToolResult, TurnEnd,
};
use aphid_core::{BlockKind, Event};

use aphid_core::Transcript;

use crate::harness::{self, Harness, HarnessOptions};
use crate::plugins::scripts;
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

/// A plugin's `notify` output goes to the same stream as everything else, so it
/// interleaves correctly with the run rather than racing it.
impl aphid_agent::Sink for Printer {
    fn notify(&self, plugin: &str, text: &str) {
        self.line(&format!("[{plugin}] {text}"));
    }
}

/// Subscribes the printer to a composition.
///
/// A component rather than the printer itself, so that what it listens to and
/// what it knows how to print stay separable: `Printer` is also the
/// [`Sink`](aphid_agent::Sink) a script's `notify` reaches, and that has
/// nothing to do with the run.
pub struct Reporter {
    printer: Arc<Printer>,
    composition: Composition,
}

impl Reporter {
    #[must_use]
    pub fn new(printer: Arc<Printer>, composition: &Composition) -> Self {
        Self {
            printer,
            composition: composition.clone(),
        }
    }
}

impl Component for Reporter {
    fn name(&self) -> &str {
        "stdout-printer"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();
        let bus = Arc::clone(&self.composition.bus);

        let printer = Arc::clone(&self.printer);
        bus.on::<ToolRequest>(owner, move |request| {
            printer.line(&format!("→ {} {}", request.name, request.arguments));
        });

        // Per-line output of a running tool is the first thing to drop when
        // asked to be quiet: it is progress, not result.
        if !self.printer.quiet {
            let printer = Arc::clone(&self.printer);
            bus.on::<ToolProgress>(owner, move |progress| {
                printer.line(&format!("  │ {}", progress.chunk));
            });
        }

        let printer = Arc::clone(&self.printer);
        bus.on::<ToolResult>(owner, move |result| {
            let body = text_of(&result.content);
            let summary = body.lines().next().unwrap_or("").trim();
            let arrow = if result.is_error { "✗" } else { "←" };
            let more = match body.lines().count() {
                0 | 1 => String::new(),
                n => format!("  (+{} lines)", n - 1),
            };
            printer.line(&format!("{arrow} {} {summary}{more}", result.name));
        });

        let printer = Arc::clone(&self.printer);
        bus.on::<TurnEnd>(owner, move |_| printer.write("", false));

        let printer = Arc::clone(&self.printer);
        bus.on::<RunEnd>(owner, move |end| {
            printer.write("", false);
            let usage = end.run.usage;
            printer.line(&format!(
                "\n{} turns · in {} · cached {} · out {} · ${:.6}",
                end.turns, usage.input, usage.cache_read, usage.output, usage.cost.total
            ));
            if let Some(error) = &end.error {
                printer.line(&format!("error: {error}"));
            }
        });

        // Only assistant prose goes to stdout as it streams. Tool-call
        // arguments arrive as deltas too, and printing raw JSON token by token
        // helps nobody — the call is announced in full when it is requested.
        let printer = Arc::clone(&self.printer);
        self.composition.stream.subscribe(owner, move |event, cx| {
            if let Event::Delta {
                kind: BlockKind::Text,
                span,
                ..
            } = *event
            {
                printer.write(cx.text(span), true);
            }
        });

        let bus = Arc::clone(&self.composition.bus);
        let stream = Arc::clone(&self.composition.stream);
        ctx.effect(move || {
            Disposer::sync(move || {
                bus.unsubscribe::<ToolRequest>(owner);
                bus.unsubscribe::<ToolProgress>(owner);
                bus.unsubscribe::<ToolResult>(owner);
                bus.unsubscribe::<TurnEnd>(owner);
                bus.unsubscribe::<RunEnd>(owner);
                stream.unsubscribe(owner);
            })
        });
        Ok(())
    }
}

/// The text of a tool result, joined across its blocks.
fn text_of(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolContent::Text(text) => Some(text.as_str()),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
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
    let printer = Arc::new(Printer::new(quiet));
    options
        .composition
        .add(
            Arc::new(Reporter::new(Arc::clone(&printer), &options.composition)),
            serde_json::Value::Null,
        )
        .await
        .expect("the reporter has no dependencies and no schema");

    // Scripts print through the same printer, so their output interleaves with
    // the run instead of racing it.
    let plugin_files = std::mem::take(&mut options.plugin_files);
    let workspace = options.workspace.clone();
    let processes = std::sync::Arc::clone(&options.processes);
    let (host, plugin_problems) = scripts::load(&workspace, &plugin_files, printer, &processes);
    if !host.is_empty() {
        options
            .composition
            .mount(
                Arc::new(crate::scripting::ScriptHost::new(
                    host.clone(),
                    &options.composition,
                )),
                serde_json::Value::Null,
            )
            .expect("the script host has no dependencies and no schema");
        options.host = Some(host.clone());
    }

    let composition = options.composition.clone();
    composition
        .bus
        .emit(&mut crate::events::SessionStart(crate::events::Session {
            id: None,
            path: None,
            reason: if resumed.is_some() { "resume" } else { "new" }.to_owned(),
            restored: 0,
        }));

    let mut harness = harness::build(options);
    if let Some(transcript) = resumed {
        let restored = session::splice(&mut harness.agent, &transcript);
        eprintln!("aphid: resumed {restored} messages");
    }
    for note in &harness.notes {
        eprintln!("aphid: {note}");
        composition
            .bus
            .emit(&mut crate::events::Notice(note.clone()));
    }
    for diagnostic in &harness.diagnostics {
        eprintln!(
            "aphid: skipped skill {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        );
    }
    for problem in &plugin_problems {
        eprintln!("aphid: {problem}");
    }

    let outcome = harness.agent.prompt(prompt).await;

    // One prompt is the whole session here, so it ends as soon as the run does.
    composition
        .bus
        .emit(&mut crate::events::SessionEnd(crate::events::Session {
            id: None,
            path: None,
            reason: "end".to_owned(),
            restored: 0,
        }));
    // Session state is written back whether or not anybody listened.
    host.flush();

    (harness, outcome)
}
