//! Agent mode: the same streaming path, but looped until the model stops
//! calling tools.
//!
//! The renderer attaches as an ordinary plugin. There is no second subscription
//! mechanism — a UI is just a plugin that happens to print.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aphid_agent::rt::{Component, Composition, Context, Disposer};
use aphid_agent::{
    Agent, ToolContent, ToolCx, ToolOutcome, ToolRequest, ToolResult, TurnEnd, tool_fn,
};
use aphid_core::providers::deepseek;
use aphid_core::{Event, SimpleStreamOptions};

use crate::ProtocolArgs;
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

impl Reporter {
    /// Subscribe to a composition. Everything registered here leaves again if
    /// this component is ever unloaded.
    fn subscribe(self: &Arc<Self>, ctx: &Context, composition: &Composition) {
        let owner = ctx.uid();
        let bus = Arc::clone(&composition.bus);

        let reporter = Arc::clone(self);
        bus.on::<ToolRequest>(owner, move |request| {
            reporter.flush();
            println!(
                "  {} {} {}",
                reporter.style.cyan("→ tool"),
                reporter.style.bold(&request.name),
                reporter.style.dim(&request.arguments),
            );
        });

        let reporter = Arc::clone(self);
        bus.on::<ToolResult>(owner, move |result| {
            let body = text_of(&result.content);
            let arrow = if result.is_error {
                reporter.style.red("← error")
            } else {
                reporter.style.cyan("← result")
            };
            println!(
                "  {} {} {}",
                arrow,
                reporter.style.bold(&result.name),
                reporter.style.dim(body.trim()),
            );
        });

        let reporter = Arc::clone(self);
        bus.on::<TurnEnd>(owner, move |end| {
            reporter.flush();
            let usage = end.summary.usage;
            println!(
                "  {}",
                reporter.style.dim(&format!(
                    "turn {} · in {} · cached {} · out {} · ${:.6}",
                    end.run.turn, usage.input, usage.cache_read, usage.output, usage.cost.total,
                )),
            );
            println!();
        });

        let reporter = Arc::clone(self);
        composition.stream.subscribe(owner, move |event, cx| {
            let delta = match *event {
                Event::Delta { span, .. } => cx.text(span),
                _ => "",
            };
            if let Ok(mut printer) = reporter.printer.lock() {
                printer.event(event, delta);
            }
        });

        let stream = Arc::clone(&composition.stream);
        ctx.effect(move || {
            Disposer::sync(move || {
                bus.unsubscribe::<ToolRequest>(owner);
                bus.unsubscribe::<ToolResult>(owner);
                bus.unsubscribe::<TurnEnd>(owner);
                stream.unsubscribe(owner);
            })
        });
    }
}

/// Wraps the reporter so a composition can mount it.
struct Reporting {
    reporter: Arc<Reporter>,
    composition: Composition,
}

impl Component for Reporting {
    fn name(&self) -> &str {
        "reporter"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.reporter.subscribe(ctx, &self.composition);
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

pub async fn run(args: ProtocolArgs) -> ExitCode {
    let style = Style::detect();
    let model = if args.pro {
        deepseek::pro()
    } else {
        deepseek::flash()
    };

    let mut options = SimpleStreamOptions {
        reasoning: args.think(),
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

    let composition = Composition::new();
    composition
        .add(
            Arc::new(Reporting {
                reporter: Arc::new(Reporter::new(style.clone(), args.events)),
                composition: composition.clone(),
            }),
            serde_json::Value::Null,
        )
        .await
        .expect("the reporter has no dependencies and no schema");

    let mut builder = Agent::builder()
        .model(model.clone())
        .options(options)
        .compose(&composition);

    if let Some(system) = &args.system {
        builder = builder.system(system);
    }
    if args.tool {
        builder = builder.tool(weather_tool());
    }
    let mut agent = builder.build();

    banner(&style, &model, args.think());
    let started = Instant::now();
    let outcome = agent.prompt(&args.prompt()).await;
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
