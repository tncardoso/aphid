//! Agent mode: the same streaming path, but looped until the model stops
//! calling tools.
//!
//! The renderer attaches as an ordinary plugin. There is no second subscription
//! mechanism — a UI is just a plugin that happens to print.

use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use aphid_agent::{
    Agent, Cx, Flow, Guard, Interest, PendingCall, Plugin, ResultCx, StreamCx, ToolCx, ToolOutcome,
    TurnSummary, tool_fn,
};
use aphid_core::providers::deepseek;
use aphid_core::{Event, SimpleStreamOptions};

use crate::Args;
use crate::render::{EventPrinter, Style, banner};

/// Prints the run as it happens: protocol events through the existing
/// [`EventPrinter`], plus a line for each tool call and result.
///
/// `EventPrinter` needs `&mut self` and hooks take `&self`, so the printer sits
/// behind a mutex. Hooks are synchronous and never re-enter, so it is never
/// contended.
struct Reporter {
    printer: Mutex<EventPrinter>,
    style: Style,
}

impl Reporter {
    fn new(style: Style, verbose: bool) -> Self {
        Self {
            printer: Mutex::new(EventPrinter::new(style.clone(), verbose)),
            style,
        }
    }

    /// Close any half-written gutter line before printing something else.
    fn flush(&self) {
        if let Ok(mut printer) = self.printer.lock() {
            printer.finish();
        }
    }
}

impl Plugin for Reporter {
    fn name(&self) -> &str {
        "reporter"
    }

    fn interests(&self) -> Interest {
        Interest::EVENT | Interest::TOOL_CALL | Interest::TOOL_RESULT | Interest::TURN_END
    }

    fn on_event(&self, event: &Event, cx: &StreamCx<'_>) {
        let delta = match *event {
            Event::Delta { span, .. } => cx.text(span),
            _ => "",
        };
        if let Ok(mut printer) = self.printer.lock() {
            printer.event(event, delta);
        }
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        self.flush();
        println!(
            "  {} {} {}",
            self.style.cyan("→ tool"),
            self.style.bold(call.name()),
            self.style.dim(call.arguments()),
        );
        Guard::Allow
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        let body = outcome.text_content();
        let arrow = if outcome.is_error {
            self.style.red("← error")
        } else {
            self.style.cyan("← result")
        };
        println!(
            "  {} {} {}",
            arrow,
            self.style.bold(cx.name()),
            self.style.dim(body.trim()),
        );
    }

    fn on_turn_end(&self, cx: &mut Cx<'_>, turn: &TurnSummary) -> Flow {
        self.flush();
        let usage = turn.usage;
        println!(
            "  {}",
            self.style.dim(&format!(
                "turn {} · in {} · cached {} · out {} · ${:.6}",
                cx.turn(),
                usage.input,
                usage.cache_read,
                usage.output,
                usage.cost.total,
            )),
        );
        println!();
        Flow::Continue
    }
}

/// A demo tool with a real implementation, so the loop has something to do.
fn weather_tool() -> impl aphid_agent::ToolHandler {
    tool_fn(
        "get_weather",
        "Look up the current weather for a city.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "City name, e.g. \"Lisbon\"" },
                "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] }
            },
            "required": ["city"],
            "additionalProperties": false
        }),
        |args: serde_json::Value, _cx: ToolCx| async move {
            let city = args["city"].as_str().unwrap_or("nowhere");
            let unit = args["unit"].as_str().unwrap_or("celsius");
            let reading = if unit == "fahrenheit" {
                "72°F"
            } else {
                "22°C"
            };
            ToolOutcome::text(format!("{city}: clear skies, {reading}"))
                .with_details(serde_json::json!({ "city": city, "unit": unit }))
        },
    )
}

pub async fn run(args: Args) -> ExitCode {
    let style = Style::detect();
    let model = if args.pro {
        deepseek::pro()
    } else {
        deepseek::flash()
    };

    let mut options = SimpleStreamOptions {
        reasoning: args.think,
        ..Default::default()
    };
    options.stream.max_tokens = args.max_tokens;
    options.stream.temperature = args.temperature;

    match std::env::var(deepseek::API_KEY_ENV) {
        Ok(key) if !key.is_empty() => options.stream.request.api_key = Some(key.into()),
        _ => {
            eprintln!("aphid: {} is not set", deepseek::API_KEY_ENV);
            return ExitCode::FAILURE;
        }
    }

    let mut builder = Agent::builder()
        .model(model.clone())
        .options(options)
        .plugin(Reporter::new(style.clone(), args.events));

    if let Some(system) = &args.system {
        builder = builder.system(system);
    }
    if args.tool {
        builder = builder.tool(weather_tool());
    }
    let mut agent = builder.build();

    banner(&style, &model, args.think);
    let started = Instant::now();
    let outcome = agent.prompt(&args.prompt).await;
    let elapsed = started.elapsed();

    println!(
        "  {}",
        style.dim(&format!(
            "{} turns · in {} · out {} · ${:.6} · {:.1}s",
            outcome.turns,
            outcome.usage.input,
            outcome.usage.output,
            outcome.usage.cost.total,
            elapsed.as_secs_f64(),
        )),
    );

    if let Some(error) = &outcome.error {
        eprintln!("\naphid: {error}");
    }
    if outcome.is_failure() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
