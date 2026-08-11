//! The `aphid` binary: five front ends over one harness.
//!
//! `aphid` is the coding agent, and the default — everything else is a
//! subcommand. `alate` runs a resident agent, and attaches a terminal to one
//! already running. `raw` streams one completion and prints every protocol
//! event as it fires. `agent` runs the plain agent loop with a demo tool.
//! `model` manages `~/.aphid/models.json`.
//!
//! The debugging front ends exist because the interesting failures are on the
//! wire: `raw --events` shows each delta with its span, and `raw --request`
//! prints the encoded body without sending it.

mod alate;
mod code;
mod model;
mod render;

mod agent;

use std::pin::Pin;
use std::process::ExitCode;
use std::task::Context;
use std::time::Instant;

use aphid_core::api::{self, CompletionStream};
use aphid_core::providers::deepseek;
use aphid_core::{AssistantStream, Event, SimpleStreamOptions, ThinkingLevel, Tool, Transcript};
use clap::{Parser, Subcommand, ValueEnum};
use futures_core::Stream;

use render::{Style, banner, summary};

/// A fast and hackable agent harness.
#[derive(Debug, Parser)]
#[command(
    name = "aphid",
    version,
    about = "A coding agent, and the protocol-level tools to debug it",
    // The coding agent's own options are the top level, so `aphid <prompt>`
    // needs no subcommand. They cannot be combined with one.
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(flatten)]
    code: code::Args,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run or attach to a resident agent
    #[command(subcommand)]
    Alate(alate::Command),
    /// Stream a single completion, printing protocol events
    Raw(ProtocolArgs),
    /// Run the plain agent loop with a demo tool
    Agent(ProtocolArgs),
    /// Manage the models in ~/.aphid/models.json
    #[command(alias = "models", subcommand)]
    Model(model::Command),
}

/// The options `raw` and `agent` share.
#[derive(Debug, clap::Args)]
pub struct ProtocolArgs {
    /// The prompt
    #[arg(value_name = "PROMPT", required = true)]
    prompt: Vec<String>,
    /// Use deepseek-v4-pro (default: deepseek-v4-flash)
    #[arg(long)]
    pro: bool,
    /// Prepend a system message
    #[arg(long, value_name = "TEXT")]
    system: Option<String>,
    /// How hard to think
    #[arg(long, value_name = "LEVEL")]
    think: Option<Think>,
    /// Cap the response length
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,
    /// Sampling temperature
    #[arg(long, value_name = "F")]
    temperature: Option<f32>,
    /// Offer a demo `get_weather` tool, to see tool-call deltas
    #[arg(long)]
    tool: bool,
    /// Print every Delta event with its span, instead of the text
    #[arg(long)]
    events: bool,
    /// Print the encoded request body and exit (single-shot only)
    #[arg(long)]
    request: bool,
}

impl ProtocolArgs {
    fn prompt(&self) -> String {
        self.prompt.join(" ")
    }

    fn think(&self) -> Option<ThinkingLevel> {
        self.think.and_then(Think::level)
    }
}

/// How much reasoning to ask for, on the command line.
///
/// `off` is the absence of a level rather than a seventh one, which is why this
/// maps to `Option<ThinkingLevel>` rather than to the enum directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Think {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Think {
    #[must_use]
    pub fn level(self) -> Option<ThinkingLevel> {
        Some(match self {
            Think::Off => return None,
            Think::Minimal => ThinkingLevel::Minimal,
            Think::Low => ThinkingLevel::Low,
            Think::Medium => ThinkingLevel::Medium,
            Think::High => ThinkingLevel::High,
            Think::Xhigh => ThinkingLevel::XHigh,
            Think::Max => ThinkingLevel::Max,
        })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The coding agent needs more than one worker: a permission prompt blocks
    // the agent's task until the UI answers on another one. So does an alate,
    // for the same reason and also because its memory runs on a blocking task.
    // Nothing else does.
    let runtime = match cli.command {
        None | Some(Command::Alate(_)) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build(),
        Some(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
    };
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("aphid: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        None => runtime.block_on(code::run(cli.code)),
        Some(Command::Alate(command)) => runtime.block_on(alate::run(command)),
        Some(Command::Raw(args)) => runtime.block_on(run(args)),
        Some(Command::Agent(args)) => runtime.block_on(agent::run(args)),
        Some(Command::Model(command)) => runtime.block_on(model::run(command)),
    }
}

async fn run(args: ProtocolArgs) -> ExitCode {
    let style = Style::detect();
    let model = if args.pro {
        deepseek::pro()
    } else {
        deepseek::flash()
    };
    let tools: Vec<Tool> = if args.tool {
        vec![weather_tool()]
    } else {
        Vec::new()
    };

    let mut transcript = Transcript::new();
    if let Some(system) = &args.system {
        transcript.push_system(system);
    }
    transcript.push_user(&args.prompt());

    let mut options = SimpleStreamOptions {
        reasoning: args.think(),
        ..Default::default()
    };
    options.stream.max_tokens = args.max_tokens;
    options.stream.temperature = args.temperature;

    if args.request {
        match CompletionStream::preview_request(&model, &transcript, &tools, &options) {
            Ok(body) => {
                println!("{}", pretty_json(&body));
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("aphid: could not encode the request: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    match std::env::var(deepseek::API_KEY_ENV) {
        Ok(key) if !key.is_empty() => options.stream.request.api_key = Some(key.into()),
        _ => {
            eprintln!("aphid: {} is not set", deepseek::API_KEY_ENV);
            return ExitCode::FAILURE;
        }
    }

    banner(&style, &model, args.think());
    let started = Instant::now();
    let mut stream = api::stream(&model, &transcript, &tools, &options).await;
    let mut printer = render::EventPrinter::new(style.clone(), args.events);

    while let Some(event) = next(&mut stream).await {
        printer.event(&event, delta_text(&event, &stream));
    }
    printer.finish();

    let elapsed = started.elapsed();
    let turn = stream.finish();
    let stop = turn.meta().stop_reason;
    let failure = turn.meta().error_message.clone();

    // The whole point of the staging buffer: one memcpy, and the turn is part of
    // the conversation.
    let id = transcript.commit(turn);
    let message = transcript.message(id);
    summary(&style, &message, elapsed);

    if let Some(error) = failure {
        eprintln!("\naphid: {error}");
    }
    if stop.is_failure() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `futures_core` gives us the trait but no combinators, and one `poll_fn` is
/// cheaper than depending on `futures-util`.
async fn next<S: Stream<Item = Event> + Unpin>(stream: &mut S) -> Option<Event> {
    std::future::poll_fn(|cx: &mut Context<'_>| Pin::new(&mut *stream).poll_next(cx)).await
}

/// A tool that exists only to make tool-call deltas visible.
fn weather_tool() -> Tool {
    Tool::new(
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
    )
}

fn pretty_json(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| body.to_owned())
}

/// Resolve the bytes an [`Event::Delta`] names; everything else carries no text.
fn delta_text<'s>(event: &Event, stream: &'s impl AssistantStream) -> &'s str {
    match *event {
        Event::Delta { span, .. } => stream.text(span),
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_word_is_a_prompt_for_the_coding_agent() {
        let cli = Cli::parse_from(["aphid", "fix", "the", "test"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.code.prompt().as_deref(), Some("fix the test"));
    }

    #[test]
    fn no_arguments_opens_the_ui() {
        let cli = Cli::parse_from(["aphid"]);
        assert!(cli.command.is_none());
        assert!(cli.code.prompt().is_none());
    }

    #[test]
    fn a_leading_subcommand_wins_over_the_prompt() {
        let cli = Cli::parse_from(["aphid", "raw", "--pro", "hi"]);
        let Some(Command::Raw(args)) = cli.command else {
            panic!("expected raw");
        };
        assert!(args.pro);
        assert_eq!(args.prompt(), "hi");
    }

    #[test]
    fn off_is_the_absence_of_a_thinking_level() {
        let cli = Cli::parse_from(["aphid", "-p", "hi", "--think", "off"]);
        assert_eq!(cli.code.think, Some(Think::Off));
        assert_eq!(cli.code.think.and_then(Think::level), None);
    }

    #[test]
    fn resume_takes_an_optional_id() {
        let cli = Cli::parse_from(["aphid", "--resume"]);
        assert_eq!(cli.code.resume, Some(None));

        let cli = Cli::parse_from(["aphid", "--resume", "20260810T012035-0000"]);
        assert_eq!(
            cli.code.resume,
            Some(Some("20260810T012035-0000".to_owned()))
        );

        let cli = Cli::parse_from(["aphid"]);
        assert_eq!(cli.code.resume, None);
    }

    #[test]
    fn models_is_an_alias_for_model() {
        let cli = Cli::parse_from(["aphid", "models", "update"]);
        assert!(matches!(
            cli.command,
            Some(Command::Model(model::Command::Update { .. }))
        ));
    }

    #[test]
    fn a_qualified_name_reaches_model_add() {
        let cli = Cli::parse_from(["aphid", "model", "add", "deepseek/deepseek-v4-pro"]);
        let Some(Command::Model(model::Command::Add(args))) = cli.command else {
            panic!("expected model add");
        };
        assert_eq!(args.name, "deepseek/deepseek-v4-pro");
    }
}
