//! Streams one DeepSeek completion and prints every protocol event as it fires.
//!
//! This is the smallest thing that exercises the whole path: encode a request
//! out of a [`Transcript`] arena, stream the response, resolve each delta span,
//! and commit the finished turn back into the transcript.

mod agent;
mod render;

use std::pin::Pin;
use std::process::ExitCode;
use std::task::Context;
use std::time::Instant;

use aphid_core::api::{self, CompletionStream};
use aphid_core::providers::deepseek;
use aphid_core::{AssistantStream, Event, SimpleStreamOptions, ThinkingLevel, Tool, Transcript};
use futures_core::Stream;

use render::{Style, banner, summary};

const USAGE: &str = "\
aphid — stream a DeepSeek completion and print the protocol events

USAGE:
    aphid [OPTIONS] <prompt>...          stream a single completion
    aphid agent [OPTIONS] <prompt>...    run the agent loop, executing tools

OPTIONS:
    --pro                 use deepseek-v4-pro (default: deepseek-v4-flash)
    --system <text>       prepend a system message
    --think <level>       minimal | low | medium | high | xhigh | max
    --max-tokens <n>      cap the response length
    --temperature <f>     sampling temperature
    --tool                offer a demo `get_weather` tool, to see tool-call deltas
    --events              print every Delta event with its span, instead of the text
    --request             print the encoded request body and exit (single-shot only)
    -h, --help            show this help

ENVIRONMENT:
    DEEPSEEK_API_KEY      required, unless --request is given
";

fn main() -> ExitCode {
    // `agent` is a subcommand, so it only counts as one when it comes first.
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let agent_mode = argv.first().is_some_and(|word| word == "agent");
    if agent_mode {
        argv.remove(0);
    }

    let args = match Args::parse(argv.into_iter()) {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("aphid: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("aphid: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    if agent_mode {
        runtime.block_on(agent::run(args))
    } else {
        runtime.block_on(run(args))
    }
}

async fn run(args: Args) -> ExitCode {
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
    transcript.push_user(&args.prompt);

    let mut options = SimpleStreamOptions {
        reasoning: args.think,
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

    banner(&style, &model, args.think);
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

pub struct Args {
    prompt: String,
    system: Option<String>,
    think: Option<ThinkingLevel>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    pro: bool,
    tool: bool,
    events: bool,
    request: bool,
}

impl Args {
    /// `Ok(None)` means help was requested.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut prompt: Vec<String> = Vec::new();
        let mut parsed = Args {
            prompt: String::new(),
            system: None,
            think: None,
            max_tokens: None,
            temperature: None,
            pro: false,
            tool: false,
            events: false,
            request: false,
        };
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--pro" => parsed.pro = true,
                "--tool" => parsed.tool = true,
                "--events" => parsed.events = true,
                "--request" => parsed.request = true,
                "--system" => parsed.system = Some(value(&mut args, "--system")?),
                "--think" => parsed.think = Some(thinking_level(&value(&mut args, "--think")?)?),
                "--max-tokens" => {
                    let raw = value(&mut args, "--max-tokens")?;
                    parsed.max_tokens = Some(
                        raw.parse()
                            .map_err(|_| format!("`{raw}` is not a token count"))?,
                    );
                }
                "--temperature" => {
                    let raw = value(&mut args, "--temperature")?;
                    parsed.temperature = Some(
                        raw.parse()
                            .map_err(|_| format!("`{raw}` is not a number"))?,
                    );
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown option `{other}`"));
                }
                word => prompt.push(word.to_owned()),
            }
        }

        if prompt.is_empty() {
            return Err("a prompt is required".to_owned());
        }
        parsed.prompt = prompt.join(" ");
        Ok(Some(parsed))
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

fn thinking_level(raw: &str) -> Result<ThinkingLevel, String> {
    Ok(match raw {
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        "max" => ThinkingLevel::Max,
        other => return Err(format!("`{other}` is not a thinking level")),
    })
}

/// Resolve the bytes an [`Event::Delta`] names; everything else carries no text.
fn delta_text<'s>(event: &Event, stream: &'s impl AssistantStream) -> &'s str {
    match *event {
        Event::Delta { span, .. } => stream.text(span),
        _ => "",
    }
}
