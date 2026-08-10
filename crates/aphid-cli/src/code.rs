//! `aphid` — the coding agent, interactive or headless.

use std::path::PathBuf;
use std::process::ExitCode;

use aphid_code::harness::HarnessOptions;
use aphid_code::model::Catalog;
use aphid_code::plugins::scripts;
use aphid_code::plugins::{DenyAll, Permissions};
use aphid_code::session::{self, sessions_dir};
use aphid_code::{Workspace, headless, tui};
use aphid_core::ThinkingLevel;
use aphid_core::providers::deepseek;

use crate::Think;

/// The coding agent's options.
///
/// Flattened into the top-level command, so `aphid <prompt>` needs no
/// subcommand at all.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Run headless: stream to stdout and exit
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub print: Option<String>,
    /// The prompt. Given one, aphid runs headless rather than opening the UI
    #[arg(value_name = "PROMPT")]
    pub words: Vec<String>,
    /// Model id, or a unique part of one (default: the first known)
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,
    /// List the known models and exit
    #[arg(long = "models")]
    pub list_models: bool,
    /// How hard to think
    #[arg(long, value_name = "LEVEL")]
    pub think: Option<Think>,
    /// Replace the built-in instructions
    #[arg(long, value_name = "TEXT")]
    pub system: Option<String>,
    /// Add to the instructions
    #[arg(long, value_name = "TEXT")]
    pub append_system: Option<String>,
    /// Continue the newest session here, or one named by id
    #[arg(long, value_name = "ID", num_args = 0..=1)]
    pub resume: Option<Option<String>>,
    /// List saved sessions for this workspace and exit
    #[arg(long = "sessions")]
    pub list_sessions: bool,
    /// Ask before running anything that changes the workspace
    #[arg(long)]
    pub confirm: bool,
    /// Skip AGENTS.md and skills
    #[arg(long)]
    pub no_context: bool,
    /// Skip every .aphid/plugins file
    #[arg(long)]
    pub no_plugins: bool,
    /// Load one plugin from a path, on top of whatever was discovered
    #[arg(long = "plugin", value_name = "PATH")]
    pub plugins: Vec<PathBuf>,
    /// List the plugins that would load and exit
    #[arg(long = "list-plugins")]
    pub list_plugins: bool,
    /// Stop a run after this many provider requests
    #[arg(long, value_name = "N")]
    pub max_turns: Option<u32>,
    /// Headless: drop the line-by-line output of running tools
    #[arg(long)]
    pub quiet: bool,
}

impl Args {
    /// The prompt, however it was given.
    ///
    /// `-p` and bare words mean the same thing and always have: either one runs
    /// headless. Only an empty prompt opens the terminal UI.
    #[must_use]
    pub fn prompt(&self) -> Option<String> {
        if let Some(prompt) = &self.print {
            return Some(prompt.clone());
        }
        if self.words.is_empty() {
            return None;
        }
        Some(self.words.join(" "))
    }

    #[must_use]
    pub fn thinking(&self) -> Option<ThinkingLevel> {
        self.think.and_then(Think::level)
    }
}

pub async fn run(args: Args) -> ExitCode {
    let catalog = Catalog::new();
    for diagnostic in catalog.diagnostics() {
        eprintln!("aphid: {diagnostic}");
    }

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

    let plugin_files = collect_plugins(&workspace, args.no_plugins, &args.plugins);

    if args.list_plugins {
        if plugin_files.is_empty() {
            println!("no plugins");
        }
        for file in &plugin_files {
            let scope = if file.project { "project" } else { "global" };
            println!(
                "{:<20} {:<8} {}",
                file.name,
                scope,
                file.description.as_deref().unwrap_or("")
            );
        }
        return ExitCode::SUCCESS;
    }

    let thinking = args.thinking();

    let api_key = match api_key(&model) {
        Ok(key) => key,
        Err(message) => {
            eprintln!("aphid: {message}");
            return ExitCode::FAILURE;
        }
    };

    let prompt = args.prompt();

    let mut options = HarnessOptions::new(workspace.clone());
    options.model = model;
    options.thinking = thinking;
    options.system = args.system;
    options.append_system = args.append_system;
    options.load_context = !args.no_context;
    options.plugin_files = plugin_files;
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

    match prompt {
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

/// The plugins to load: whatever was discovered, plus anything named on the
/// command line.
///
/// An explicit `--plugin` is honoured even under `--no-plugins`, because the two
/// say different things: one turns off discovery, the other names a file.
fn collect_plugins(
    workspace: &Workspace,
    no_plugins: bool,
    explicit: &[PathBuf],
) -> Vec<aphid_plugin::PluginFile> {
    let mut files = Vec::new();

    if !no_plugins {
        let (discovered, problems) =
            scripts::discover(workspace, aphid_code::home_dir().as_deref());
        for problem in problems {
            eprintln!("aphid: {problem}");
        }
        files = discovered;
    }

    for path in explicit {
        match aphid_plugin::explicit(path) {
            Ok(file) => {
                files.retain(|existing| existing.name != file.name);
                files.push(file);
            }
            Err(problem) => eprintln!("aphid: {problem}"),
        }
    }

    files
}

/// The key for this model, from the variable the model itself names.
///
/// Carried on the model rather than fixed at the provider, so adding an OpenAI
/// or Zhipu model to `~/.aphid/models.json` reads that provider's variable
/// instead of DeepSeek's.
fn api_key(model: &aphid_core::Model) -> Result<String, String> {
    let variable = model
        .api_key_env
        .as_deref()
        .unwrap_or(deepseek::API_KEY_ENV);
    match std::env::var(variable) {
        Ok(key) if !key.is_empty() => Ok(key),
        _ => Err(format!("{variable} is not set, and {} needs it", model.id)),
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
