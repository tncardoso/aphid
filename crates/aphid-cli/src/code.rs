//! `aphid` — the coding agent, interactive or headless.

use std::path::PathBuf;
use std::process::ExitCode;

use aphid_code::harness::HarnessOptions;
use aphid_code::model::Catalog;
use aphid_code::plugins::{DenyAll, Permissions};
use aphid_code::session::{self, sessions_dir};
use aphid_code::{Workspace, headless, tui};
use aphid_core::{ThinkingLevel, providers::deepseek};

pub const USAGE: &str = "\
aphid — a coding agent

USAGE:
    aphid [OPTIONS]                 open the terminal UI
    aphid [OPTIONS] -p <prompt>     run one prompt and print the result
    aphid raw   [OPTIONS] <prompt>  stream a single completion, printing protocol events
    aphid agent [OPTIONS] <prompt>  run the plain agent loop with a demo tool

OPTIONS:
    -p, --print <prompt>  run headless: stream to stdout and exit
    --model <name>        model id, or a unique part of one (default: the first known)
    --models              list the known models and exit
    --think <level>       off | minimal | low | medium | high | xhigh | max
    --system <text>       replace the built-in instructions
    --append-system <t>   add to the instructions
    --resume [id]         continue the newest session here, or one named by id
    --sessions            list saved sessions for this workspace and exit
    --confirm             ask before running anything that changes the workspace
    --no-context          skip AGENTS.md and skills
    --max-turns <n>       stop a run after this many provider requests
    --quiet               headless: drop the line-by-line output of running tools
    -h, --help            show this help

ENVIRONMENT:
    DEEPSEEK_API_KEY      required
";

pub struct Args {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub think: Option<String>,
    pub system: Option<String>,
    pub append_system: Option<String>,
    /// `Some(None)` is `--resume` with no id: the newest session here.
    pub resume: Option<Option<String>>,
    pub confirm: bool,
    pub no_context: bool,
    pub max_turns: Option<u32>,
    pub quiet: bool,
    pub list_models: bool,
    pub list_sessions: bool,
}

impl Args {
    /// `Ok(None)` means help was requested.
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut parsed = Args {
            prompt: None,
            model: None,
            think: None,
            system: None,
            append_system: None,
            resume: None,
            confirm: false,
            no_context: false,
            max_turns: None,
            quiet: false,
            list_models: false,
            list_sessions: false,
        };
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--confirm" => parsed.confirm = true,
                "--no-context" => parsed.no_context = true,
                "--quiet" => parsed.quiet = true,
                "--models" => parsed.list_models = true,
                "--sessions" => parsed.list_sessions = true,
                "-p" | "--print" => parsed.prompt = Some(value(&mut args, "--print")?),
                "--model" => parsed.model = Some(value(&mut args, "--model")?),
                "--think" => parsed.think = Some(value(&mut args, "--think")?),
                "--system" => parsed.system = Some(value(&mut args, "--system")?),
                "--append-system" => {
                    parsed.append_system = Some(value(&mut args, "--append-system")?);
                }
                "--max-turns" => {
                    let raw = value(&mut args, "--max-turns")?;
                    parsed.max_turns =
                        Some(raw.parse().map_err(|_| format!("`{raw}` is not a count"))?);
                }
                "--resume" => {
                    // The id is optional, so only take the next word when it is
                    // not another flag.
                    let id = match args.peek() {
                        Some(next) if !next.starts_with('-') => args.next(),
                        _ => None,
                    };
                    parsed.resume = Some(id);
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option `{other}`"));
                }
                // A bare word is the prompt, so `aphid "fix the test"` works.
                word => match &mut parsed.prompt {
                    Some(prompt) => {
                        prompt.push(' ');
                        prompt.push_str(word);
                    }
                    None => parsed.prompt = Some(word.to_owned()),
                },
            }
        }

        Ok(Some(parsed))
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

fn thinking_level(raw: &str) -> Result<Option<ThinkingLevel>, String> {
    Ok(match raw {
        "off" | "none" => None,
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        other => return Err(format!("`{other}` is not a thinking level")),
    })
}

pub async fn run(args: Args) -> ExitCode {
    let catalog = Catalog::new();

    if args.list_models {
        for model in catalog.models() {
            println!(
                "{:<24} {} ctx · ${:.2}/${:.2} per M tokens",
                model.id, model.context_window, model.cost.rates.input, model.cost.rates.output
            );
        }
        return ExitCode::SUCCESS;
    }

    let workspace = Workspace::discover();

    if args.list_sessions {
        let directory = sessions_dir(&workspace);
        let sessions = session::list(&directory);
        if sessions.is_empty() {
            println!("no sessions in {}", directory.display());
        }
        for summary in sessions {
            println!(
                "{}  {:>4} messages  {}",
                summary.header.id, summary.messages, summary.header.cwd
            );
        }
        return ExitCode::SUCCESS;
    }

    let model = match &args.model {
        Some(name) => match catalog.resolve(name) {
            Ok(model) => model,
            Err(error) => {
                eprintln!("aphid: --model {name}: {error}");
                return ExitCode::from(2);
            }
        },
        None => catalog.default_model(),
    };

    let thinking = match args.think.as_deref().map(thinking_level).transpose() {
        Ok(level) => level.flatten(),
        Err(message) => {
            eprintln!("aphid: {message}");
            return ExitCode::from(2);
        }
    };

    let api_key = match std::env::var(deepseek::API_KEY_ENV) {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("aphid: {} is not set", deepseek::API_KEY_ENV);
            return ExitCode::FAILURE;
        }
    };

    let mut options = HarnessOptions::new(workspace.clone());
    options.model = model;
    options.thinking = thinking;
    options.system = args.system;
    options.append_system = args.append_system;
    options.load_context = !args.no_context;
    options.api_key = Some(api_key.into());
    if let Some(max_turns) = args.max_turns {
        options.max_turns = max_turns;
    }

    let resume = match resolve_resume(&workspace, &options.cwd, args.resume.as_ref()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("aphid: {message}");
            return ExitCode::from(2);
        }
    };

    match args.prompt {
        // Headless has no terminal to prompt at, so the gate refuses rather
        // than silently allowing what `--confirm` was meant to stop.
        Some(prompt) => {
            if args.confirm {
                options
                    .plugins
                    .push(std::sync::Arc::new(Permissions::new(std::sync::Arc::new(
                        DenyAll,
                    ))));
            }

            // Headless runs are recorded too, so `--sessions` and `--resume`
            // see them the same way they see interactive ones.
            let model_id = options.model.id.to_string();
            let (store, resumed) = match session::attach(
                &sessions_dir(&workspace),
                &options.cwd,
                Some(&model_id),
                resume.as_deref(),
            ) {
                Ok(attached) => attached,
                Err(error) => {
                    eprintln!("aphid: could not open the session: {error}");
                    return ExitCode::FAILURE;
                }
            };
            options.plugins.push(store);

            let (_harness, outcome) = headless::run(options, &prompt, args.quiet, resumed).await;
            if outcome.is_failure() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        None => match tui::run(options, resume, args.confirm).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("aphid: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn resolve_resume(
    workspace: &Workspace,
    cwd: &std::path::Path,
    resume: Option<&Option<String>>,
) -> Result<Option<PathBuf>, String> {
    let Some(request) = resume else {
        return Ok(None);
    };
    let directory = sessions_dir(workspace);

    let found = match request {
        Some(id) => session::resolve(&directory, id)
            .ok_or_else(|| format!("no session matching `{id}` in {}", directory.display()))?,
        None => session::newest_for(&directory, cwd)
            .ok_or_else(|| format!("no session for {} yet", cwd.display()))?,
    };
    Ok(Some(found.path))
}
