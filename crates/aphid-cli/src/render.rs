//! Terminal rendering for the event stream.

use std::io::{StdoutLock, Write};
use std::time::Duration;

use aphid_core::{BlockKind, Event, MessageRef, Model, StopReason, ThinkingLevel};

/// ANSI styling, disabled when the output is not a terminal or `NO_COLOR` is set.
#[derive(Clone)]
pub struct Style {
    color: bool,
}

impl Style {
    pub fn detect() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").is_ok_and(|term| term != "dumb");
        Self { color }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }
}

pub fn banner(style: &Style, model: &Model, think: Option<ThinkingLevel>) {
    let thinking = match think {
        Some(level) => level.as_str(),
        None => "off",
    };
    println!(
        "\n{} {}  {}",
        style.dim("→"),
        style.bold(&model.id),
        style.dim(&format!("{}  · thinking {thinking}", model.base_url)),
    );
    println!();
}

/// How much a single block streamed. Kept per block, because providers
/// interleave: reasoning and a tool call can both be open at once.
#[derive(Default)]
struct BlockStats {
    deltas: u32,
    bytes: usize,
}

/// Prints each protocol event, resolving delta spans against the live stream.
pub struct EventPrinter {
    style: Style,
    verbose: bool,
    /// Set while the cursor sits mid-line inside a gutter line.
    line_open: bool,
    stats: Vec<(u32, BlockStats)>,
}

impl EventPrinter {
    pub fn new(style: Style, verbose: bool) -> Self {
        Self {
            style,
            verbose,
            line_open: false,
            stats: Vec::new(),
        }
    }

    /// `delta` is the text named by an [`Event::Delta`] span, already resolved
    /// by the caller — the printer never sees the stream, so it works the same
    /// against a raw `CompletionStream` and against an agent's event hook.
    pub fn event(&mut self, event: &Event, delta: &str) {
        let mut out = std::io::stdout().lock();
        match *event {
            Event::Start => {
                let text = self.style.dim("Start");
                self.line(&mut out, &text);
            }
            Event::BlockStart { index, kind } => {
                self.stats.push((index, BlockStats::default()));
                let text = self
                    .style
                    .cyan(&format!("BlockStart {index}  {}", kind_name(kind)));
                self.line(&mut out, &text);
            }
            Event::Delta { index, kind, span } => {
                let text = delta;
                let stats = self.stats_for(index);
                stats.deltas += 1;
                stats.bytes += text.len();
                if self.verbose {
                    let label = self
                        .style
                        .dim(&format!("Delta      {index}  {span:?}  {text:?}"));
                    self.line(&mut out, &label);
                } else {
                    self.write_content(&mut out, text, kind);
                }
            }
            Event::BlockEnd { index } => {
                let stats = self.stats_for(index);
                let stats = format!("{} deltas · {} B", stats.deltas, stats.bytes);
                let text = format!(
                    "{}  {}",
                    self.style.cyan(&format!("BlockEnd   {index}")),
                    self.style.dim(&stats)
                );
                self.line(&mut out, &text);
            }
            Event::Done { stop } => {
                let text = self.style.bold(&format!("Done  {}", stop_name(stop)));
                self.line(&mut out, &text);
            }
            Event::Error { stop } => {
                let text = self.style.red(&format!("Error  {}", stop_name(stop)));
                self.line(&mut out, &text);
            }
        }
        let _ = out.flush();
    }

    pub fn finish(&mut self) {
        let mut out = std::io::stdout().lock();
        self.end_content(&mut out);
        let _ = out.flush();
    }

    /// Print one event line, first closing any gutter line left open by a delta.
    fn line(&mut self, out: &mut StdoutLock<'_>, text: &str) {
        self.end_content(out);
        let _ = writeln!(out, "  {text}");
    }

    /// Stream block text with a gutter, so multi-line replies stay readable.
    fn write_content(&mut self, out: &mut StdoutLock<'_>, text: &str, kind: BlockKind) {
        let gutter = self.style.dim(if matches!(kind, BlockKind::Thinking) {
            "  ┊ "
        } else {
            "  │ "
        });
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                let _ = writeln!(out);
                self.line_open = false;
            }
            if part.is_empty() {
                continue;
            }
            if !self.line_open {
                let _ = write!(out, "{gutter}");
                self.line_open = true;
            }
            let _ = write!(out, "{part}");
        }
    }

    fn end_content(&mut self, out: &mut StdoutLock<'_>) {
        if self.line_open {
            let _ = writeln!(out);
            self.line_open = false;
        }
    }

    fn stats_for(&mut self, index: u32) -> &mut BlockStats {
        if let Some(pos) = self.stats.iter().position(|(i, _)| *i == index) {
            return &mut self.stats[pos].1;
        }
        self.stats.push((index, BlockStats::default()));
        &mut self.stats.last_mut().expect("just pushed").1
    }
}

pub fn summary(style: &Style, message: &MessageRef<'_>, elapsed: Duration) {
    let Some(meta) = message.assistant() else {
        return;
    };
    let usage = &meta.usage;
    let reasoning = match usage.reasoning {
        Some(tokens) => format!(" (reasoning {tokens})"),
        None => String::new(),
    };
    let line = format!(
        "in {} · cached {} · out {}{} · ${:.6} · {:.1}s",
        usage.input,
        usage.cache_read,
        usage.output,
        reasoning,
        usage.cost.total,
        elapsed.as_secs_f64(),
    );
    println!("\n  {}", style.dim(&line));
}

fn kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Text => "Text",
        BlockKind::Thinking => "Thinking",
        BlockKind::Image => "Image",
        BlockKind::ToolCall => "ToolCall",
    }
}

fn stop_name(stop: StopReason) -> &'static str {
    match stop {
        StopReason::Pending => "Pending",
        StopReason::Stop => "Stop",
        StopReason::Length => "Length",
        StopReason::ToolUse => "ToolUse",
        StopReason::Error => "Error",
        StopReason::Aborted => "Aborted",
    }
}
