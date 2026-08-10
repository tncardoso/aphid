//! Assembling a coding agent.
//!
//! Everything discovered at startup — the workspace, the project's instructions,
//! its skills, the model — is resolved here and handed to
//! [`aphid_agent::Agent`]. Callers get the agent plus what was discovered, so a
//! UI can report it.

use std::path::PathBuf;
use std::sync::Arc;

use aphid_agent::{Agent, Plugin, StreamFn};
use aphid_core::{Model, ThinkingLevel};
use compact_str::CompactString;

use crate::context::{self, ContextFile};
use crate::model::{self, Catalog};
use crate::prompt::{self, PromptOptions};
use crate::skills::{self, Diagnostic, Skill};
use crate::tools::{self, Workspace};

/// How to build the harness.
pub struct HarnessOptions {
    pub workspace: Workspace,
    /// Where the user actually is, which may be below the workspace root.
    pub cwd: PathBuf,
    pub model: Model,
    pub thinking: Option<ThinkingLevel>,
    /// Replaces the built-in instructions.
    pub system: Option<String>,
    /// Appended to whichever instructions are used.
    pub append_system: Option<String>,
    /// Load `AGENTS.md` files and skills. Off makes startup fully predictable.
    pub load_context: bool,
    pub max_turns: u32,
    pub api_key: Option<CompactString>,
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// Rhai plugin files to load. The front end loads them rather than
    /// [`build`], because loading needs a [`Sink`](aphid_plugin::Sink) and only
    /// the front end knows where its output can safely go.
    pub plugin_files: Vec<aphid_plugin::PluginFile>,
    /// The loaded scripts. Set by the front end, which does the loading.
    pub host: Option<Arc<aphid_plugin::PluginHost>>,
    /// Replace the provider backend. `None` talks to the real provider; tests
    /// and replays pass a scripted one.
    pub stream_fn: Option<StreamFn>,
}

impl HarnessOptions {
    /// Defaults for a workspace: the catalog's first model, medium thinking,
    /// project context on.
    #[must_use]
    pub fn new(workspace: Workspace) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| workspace.root().to_path_buf());
        Self {
            workspace,
            cwd,
            model: Catalog::new().default_model(),
            thinking: Some(ThinkingLevel::Medium),
            system: None,
            append_system: None,
            load_context: true,
            max_turns: aphid_agent::DEFAULT_MAX_TURNS,
            api_key: None,
            plugins: Vec::new(),
            plugin_files: Vec::new(),
            host: None,
            stream_fn: None,
        }
    }
}

/// A built agent, plus what was discovered building it.
pub struct Harness {
    pub agent: Agent,
    pub workspace: Workspace,
    pub catalog: Catalog,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// Skill files that could not be loaded. Worth surfacing — a skill that
    /// silently does not exist is worse than one that reports why.
    pub diagnostics: Vec<Diagnostic>,
    /// What had to be adjusted, such as a thinking level the model cannot serve.
    pub notes: Vec<String>,
}

/// Build a coding agent.
#[must_use]
pub fn build(options: HarnessOptions) -> Harness {
    let HarnessOptions {
        workspace,
        cwd,
        model,
        thinking,
        system,
        append_system,
        load_context,
        max_turns,
        api_key,
        plugins,
        // Taken by the front end before it calls this, so anything left here
        // was never loaded and has nothing to contribute.
        plugin_files: _,
        host,
        stream_fn,
    } = options;

    let mut notes = Vec::new();

    let (context_files, skills, diagnostics) = if load_context {
        let home = context::home_dir();
        let files = context::discover(&workspace, &cwd, home.as_deref());
        let (skills, diagnostics) = skills::discover(&workspace, home.as_deref());
        (files, skills, diagnostics)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    let prompt_options = PromptOptions {
        custom: system,
        append: append_system,
        tools: tools::snippets()
            .into_iter()
            .map(|(name, snippet)| (name.to_owned(), snippet.to_owned()))
            .collect(),
        guidelines: Vec::new(),
        context_files: context_files.clone(),
        skills: skills.clone(),
    };
    let mut system_prompt = prompt::build(&prompt_options, &cwd);
    // The last word on the prompt, and the only hook that runs before an agent
    // exists — which is why it is here and not in the loop.
    if let Some(host) = &host {
        host.system_prompt(&mut system_prompt);
    }

    let (thinking, note) = model::clamp_thinking(&model, thinking);
    if let Some(note) = note {
        notes.push(note);
    }

    // A catalog that could not be read is worth saying out loud, once, where the
    // UI and the headless runner both already print notes.
    let catalog = Catalog::new();
    notes.extend(catalog.diagnostics().iter().cloned());

    let mut builder = Agent::builder()
        .model(model)
        .system(system_prompt)
        .tools(tools::all(&workspace, host))
        .max_turns(max_turns);

    if let Some(level) = thinking {
        builder = builder.thinking(level);
    }
    if let Some(key) = api_key {
        builder = builder.api_key(key);
    }
    for plugin in plugins {
        builder = builder.plugin_arc(plugin);
    }
    if let Some(stream_fn) = stream_fn {
        builder = builder.stream_fn(stream_fn);
    }

    Harness {
        agent: builder.build(),
        workspace,
        catalog,
        context_files,
        skills,
        diagnostics,
        notes,
    }
}
