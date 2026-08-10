//! `aphid model` — the models in `~/.aphid/models.json`.
//!
//! Everything here is a small amount of glue over two `aphid-core` modules:
//! [`catalog`] owns the file, [`models_dev`] owns the document the descriptions
//! come from. This module decides what to print and which exit code to leave.

use std::process::ExitCode;
use std::time::Duration;

use aphid_code::model::Catalog;
use aphid_core::catalog::{self, CompatProfile, ModelEntry};
use aphid_core::models_dev::{self, CachePolicy, Entry, Index, Source};
use aphid_core::{Api, Model};
use clap::{Subcommand, ValueEnum};

/// Usage errors exit 2; everything else that failed exits 1. Same split the
/// coding agent uses.
const USAGE_ERROR: u8 = 2;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add a model from models.dev
    Add(AddArgs),
    /// Remove a model from the catalog
    Remove(RemoveArgs),
    /// List the models in the catalog
    List(ListArgs),
    /// Search models.dev without adding anything
    Search(SearchArgs),
    /// Refresh the cached copy of models.dev
    Update(UpdateArgs),
}

#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// `provider/model`, or a model id that only one provider serves
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Look only at this provider, when a bare model id is ambiguous
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
    /// Endpoint URL, when models.dev lists none
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Wire protocol to use anyway
    #[arg(long, value_name = "API")]
    pub api: Option<String>,
    /// Environment variable holding the API key
    #[arg(long, value_name = "VAR")]
    pub api_key_env: Option<String>,
    /// Which endpoint quirks to assume
    #[arg(long, value_name = "PROFILE")]
    pub compat: Option<Profile>,
    /// Replace an entry that is already in the catalog
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub cache: CacheArgs,
}

#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// Model id, or a unique part of one
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Show the built-in models too
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, clap::Args)]
pub struct SearchArgs {
    /// Matched against provider id, model id and model name
    #[arg(value_name = "QUERY")]
    pub query: String,
    /// Stop after this many results (default: show them all)
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    #[command(flatten)]
    pub cache: CacheArgs,
}

#[derive(Debug, clap::Args)]
pub struct UpdateArgs {}

/// How the cache should be treated for one command.
#[derive(Debug, clap::Args)]
pub struct CacheArgs {
    /// Fetch models.dev even if the cache is fresh
    #[arg(long, conflicts_with = "offline")]
    pub refresh: bool,
    /// Use the cache only, and fail if there is none
    #[arg(long)]
    pub offline: bool,
}

impl CacheArgs {
    fn policy(&self) -> CachePolicy {
        match (self.refresh, self.offline) {
            (true, _) => CachePolicy::Refresh,
            (_, true) => CachePolicy::Offline,
            _ => CachePolicy::Ttl(models_dev::DEFAULT_TTL),
        }
    }
}

/// The compatibility profiles `--compat` accepts.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    /// OpenAI's own behaviour
    Openai,
    /// A generic third-party OpenAI-compatible endpoint
    Compatible,
    /// DeepSeek
    Deepseek,
    /// No quirks table
    None,
}

impl From<Profile> for CompatProfile {
    fn from(profile: Profile) -> Self {
        match profile {
            Profile::Openai => CompatProfile::Openai,
            Profile::Compatible => CompatProfile::Compatible,
            Profile::Deepseek => CompatProfile::Deepseek,
            Profile::None => CompatProfile::None,
        }
    }
}

pub async fn run(command: Command) -> ExitCode {
    match command {
        Command::Add(args) => add(args).await,
        Command::Remove(args) => remove(&args),
        Command::List(args) => list(&args),
        Command::Search(args) => search(args).await,
        Command::Update(_) => update().await,
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

async fn add(args: AddArgs) -> ExitCode {
    let Some(path) = catalog::config_path() else {
        eprintln!("aphid: no home directory, so there is nowhere to keep the catalog");
        return ExitCode::FAILURE;
    };

    let (index, source) = match document(args.cache.policy()).await {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };

    // `--provider` narrows before resolution, so a bare id that several
    // providers serve stops being ambiguous.
    let name = match &args.provider {
        Some(provider) => format!("{provider}/{}", args.name),
        None => args.name.clone(),
    };
    let entry = match models_dev::find(&index, &name) {
        Ok(entry) => entry,
        Err(models_dev::FindError::Ambiguous { name, providers }) => {
            eprintln!(
                "aphid: `{name}` is served by {} providers:",
                providers.len()
            );
            for provider in &providers {
                eprintln!("    {provider}/{name}");
            }
            eprintln!("\nName one of them, or pass --provider <id>.");
            return ExitCode::from(USAGE_ERROR);
        }
        Err(error) => {
            eprintln!("aphid: {error}");
            eprintln!("Try `aphid model search {}`.", args.name);
            return ExitCode::from(USAGE_ERROR);
        }
    };

    let overrides = models_dev::Overrides {
        base_url: args.base_url,
        api: args
            .api
            .map(|api| api.parse::<Api>().unwrap_or(Api::OpenAiCompletions)),
        api_key_env: args.api_key_env,
        compat: args.compat.map(CompatProfile::from),
    };
    let converted = match models_dev::to_model(&entry, &overrides) {
        Ok(converted) => converted,
        Err(error) => {
            eprintln!("aphid: {error}");
            return ExitCode::from(USAGE_ERROR);
        }
    };

    let mut config = match catalog::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("aphid: {error}");
            return ExitCode::FAILURE;
        }
    };
    if config.find(&converted.model.id).is_some() && !args.force {
        eprintln!(
            "aphid: {} is already in {}. Pass --force to replace it.",
            converted.model.id,
            path.display()
        );
        return ExitCode::from(USAGE_ERROR);
    }

    let replaced = config.push_or_replace(ModelEntry::from(&converted.model));
    if let Err(error) = catalog::save(&path, &config) {
        eprintln!("aphid: could not write {}: {error}", path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "{} {} in {}",
        if replaced { "replaced" } else { "added" },
        converted.model.id,
        path.display()
    );
    describe(&converted.model);
    for note in &converted.notes {
        println!("  note: {note}");
    }
    println!("{}", provenance(source));
    ExitCode::SUCCESS
}

/// The four numbers worth checking before a first request.
fn describe(model: &Model) {
    println!("  provider  {}", model.provider);
    println!("  endpoint  {}", model.base_url);
    println!(
        "  limits    {} context · {} output",
        model.context_window, model.max_tokens
    );
    println!(
        "  price     ${:.2} in · ${:.2} out per M tokens",
        model.cost.rates.input, model.cost.rates.output
    );
    match &model.api_key_env {
        Some(variable) => println!("  key       ${variable}"),
        None => println!("  key       none recorded"),
    }
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

fn remove(args: &RemoveArgs) -> ExitCode {
    let Some(path) = catalog::config_path() else {
        eprintln!("aphid: no home directory, so there is no catalog to edit");
        return ExitCode::FAILURE;
    };
    let mut config = match catalog::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("aphid: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Resolve against the configured models alone, using the same rules
    // `--model` uses, so `aphid model remove pro` works.
    let configured = Catalog::from_parts(Vec::new(), config.models());
    let id = match configured.resolve(&args.name) {
        Ok(model) => model.id.to_string(),
        Err(error) => {
            if Catalog::new().resolve(&args.name).is_ok() {
                eprintln!(
                    "aphid: `{}` is a built-in model, not one of yours, so there is nothing to \
                     remove",
                    args.name
                );
            } else {
                eprintln!("aphid: {error}");
            }
            return ExitCode::from(USAGE_ERROR);
        }
    };

    config.remove(&id);
    if let Err(error) = catalog::save(&path, &config) {
        eprintln!("aphid: could not write {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    println!("removed {id} from {}", path.display());
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(args: &ListArgs) -> ExitCode {
    let Some(path) = catalog::config_path() else {
        eprintln!("aphid: no home directory, so there is no catalog to read");
        return ExitCode::FAILURE;
    };
    let config = match catalog::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("aphid: {error}");
            return ExitCode::FAILURE;
        }
    };

    if args.all {
        let catalog = Catalog::new();
        for model in catalog.models() {
            let origin = if config.find(model.id.as_str()).is_some() {
                "config"
            } else {
                "built-in"
            };
            row(&model.id, &model.provider.to_string(), model, origin);
        }
        return ExitCode::SUCCESS;
    }

    if config.models().is_empty() {
        println!("no models in {}", path.display());
        println!("Add one with `aphid model add <provider/model>`.");
        return ExitCode::SUCCESS;
    }
    for entry in config.models() {
        match Model::try_from(entry) {
            Ok(model) => row(&entry.id, &entry.provider, &model, "config"),
            Err(error) => println!("{:<28} {error}", entry.id),
        }
    }
    ExitCode::SUCCESS
}

fn row(id: &str, provider: &str, model: &Model, origin: &str) {
    println!(
        "{id:<28} {provider:<14} {:>9} ctx  ${:>6.2}/${:<6.2}  {origin}",
        model.context_window, model.cost.rates.input, model.cost.rates.output
    );
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

async fn search(args: SearchArgs) -> ExitCode {
    let (index, source) = match document(args.cache.policy()).await {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };

    let found = models_dev::search(&index, &args.query);
    if found.is_empty() {
        println!("nothing on models.dev matches `{}`", args.query);
        println!("{}", provenance(source));
        return ExitCode::SUCCESS;
    }

    let shown = args.limit.unwrap_or(found.len());
    for entry in found.iter().take(shown) {
        result_row(entry);
    }
    if found.len() > shown {
        println!("... and {} more", found.len() - shown);
    }
    println!("{}", provenance(source));
    ExitCode::SUCCESS
}

fn result_row(entry: &Entry<'_>) {
    let cost = entry.model.cost.as_ref();
    let input = cost.map_or(0.0, |cost| cost.input);
    let output = cost.map_or(0.0, |cost| cost.output);
    println!(
        "{:<52} {:>9} ctx  ${:>6.2}/${:<6.2}  {}",
        entry.qualified(),
        entry.model.limit.context,
        input,
        output,
        entry.model.name
    );
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

async fn update() -> ExitCode {
    let Some(path) = cache_path() else {
        return ExitCode::FAILURE;
    };

    // Read what is there before overwriting it, so the refresh can say what
    // actually changed rather than just that it happened.
    let before = models_dev::read_cache(&path)
        .and_then(|cached| models_dev::parse(&cached.body).ok())
        .map(|index| names(&index));

    let (index, _) = match document_at(&path, CachePolicy::Refresh).await {
        Ok(loaded) => loaded,
        Err(code) => return code,
    };

    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    println!(
        "{} · {} providers · {} models · {:.1} MB",
        path.display(),
        index.provider_count(),
        index.model_count(),
        size as f64 / (1024.0 * 1024.0)
    );

    match before {
        None => println!("first copy, so there is nothing to compare it against"),
        Some(before) => {
            let after = names(&index);
            let added: Vec<&String> = after.difference(&before).collect();
            let removed: Vec<&String> = before.difference(&after).collect();
            if added.is_empty() && removed.is_empty() {
                println!("no models were added or removed");
            } else {
                report_delta("added", &added);
                report_delta("removed", &removed);
            }
        }
    }
    ExitCode::SUCCESS
}

fn names(index: &Index) -> std::collections::BTreeSet<String> {
    index.entries().map(|entry| entry.qualified()).collect()
}

/// Print a change list, in full when it is short and as a count when it is not.
fn report_delta(label: &str, models: &[&String]) {
    const SHOWN: usize = 12;
    if models.is_empty() {
        return;
    }
    println!("{} {label}:", models.len());
    for name in models.iter().take(SHOWN) {
        println!("    {name}");
    }
    if models.len() > SHOWN {
        println!("    ... and {} more", models.len() - SHOWN);
    }
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

async fn document(policy: CachePolicy) -> Result<(Index, Source), ExitCode> {
    let path = cache_path().ok_or(ExitCode::FAILURE)?;
    document_at(&path, policy).await
}

/// Where the cached document lives, or a message saying why there is nowhere.
fn cache_path() -> Option<std::path::PathBuf> {
    let path = catalog::cache_path();
    if path.is_none() {
        eprintln!("aphid: no home directory, so there is nowhere to cache models.dev");
    }
    path
}

async fn document_at(
    path: &std::path::Path,
    policy: CachePolicy,
) -> Result<(Index, Source), ExitCode> {
    match models_dev::load(path, policy).await {
        Ok(loaded) => Ok(loaded),
        Err(error) => {
            eprintln!("aphid: {error}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Where the descriptions came from, so a stale answer is never a silent one.
fn provenance(source: Source) -> String {
    match source {
        Source::Network => "(fetched from models.dev)".to_owned(),
        Source::Cache { age, stale } => {
            let age = humanise(age);
            if stale {
                format!("(cached {age} ago, and models.dev could not be reached)")
            } else {
                format!("(cached {age} ago; `aphid model update` to refresh)")
            }
        }
    }
}

fn humanise(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..=90 => format!("{seconds}s"),
        91..=5400 => format!("{}m", seconds / 60),
        5401..=172_800 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}
