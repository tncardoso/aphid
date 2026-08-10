//! [models.dev](https://models.dev) as a source of model descriptions.
//!
//! models.dev publishes one JSON document describing what every provider serves:
//! context windows, prices, which reasoning efforts are on offer. Reading it is
//! how `aphid model add` fills in a [`ModelEntry`](crate::ModelEntry) the user
//! would otherwise have to write by hand.
//!
//! # The cache
//!
//! The document is several megabytes, so it is cached verbatim at
//! `~/.aphid/models.dev.json` — a plain copy of `api.json`, with no wrapper, so
//! it stays greppable and diffable. Freshness comes from the file's modification
//! time; there is no timestamp inside the file to disagree with it.
//!
//! # What does not survive the trip
//!
//! models.dev describes *models*. aphid's [`Compat`] describes *endpoints* —
//! which request fields a server rejects — and no catalog carries that. So the
//! quirks table is chosen from the provider, and only the two genuinely
//! per-model signals are read off the record: whether the model offers a
//! reasoning effort, and whether it interleaves `reasoning_content`. Everything
//! else in the profile is a starting point the user can sharpen by editing
//! `models.json`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use compact_str::CompactString;
use serde::Deserialize;

use crate::catalog::{self, CompatProfile};
use crate::compat::Compat;
use crate::model::{InputModalities, Model, ModelCost, ModelCostRates, ModelCostTier};
use crate::provider::{Api, ProviderId};
use crate::thinking::{LevelMapping, ModelThinkingLevel, ThinkingLevel, ThinkingLevelMap};

/// The published document.
pub const API_URL: &str = "https://models.dev/api.json";

/// How long a cached copy is used before aphid goes back to the network.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// Read a field that the document sometimes writes as `null`.
///
/// models.dev uses an explicit `null` where a field is simply absent — a null
/// inside a `values` array, a null string. `#[serde(default)]` alone does not
/// cover that, and one such value would otherwise fail the parse of the whole
/// six-thousand-model document.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Read a list of strings, dropping any null entries.
///
/// The same problem one level down: a `values` array can hold a `null` among
/// its efforts. There is nothing aphid could do with a nameless effort, so it
/// is dropped rather than allowed to fail the parse.
fn string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<Vec<Option<String>>> = Option::deserialize(deserializer)?;
    Ok(raw.unwrap_or_default().into_iter().flatten().collect())
}

/// The whole models.dev document, keyed by provider id.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct Index {
    providers: BTreeMap<String, Provider>,
}

/// One provider and everything it serves.
#[derive(Debug, Deserialize)]
pub struct Provider {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// Environment variables the provider's own tooling reads a key from. The
    /// first is the one aphid records.
    #[serde(default, deserialize_with = "string_list")]
    pub env: Vec<String>,
    /// Which Vercel AI SDK package talks to it, which is the only hint the
    /// document carries about the wire protocol.
    #[serde(default, deserialize_with = "nullable")]
    pub npm: String,
    /// The base URL. Absent for providers reached through an SDK rather than a
    /// plain endpoint.
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub models: BTreeMap<String, ModelRecord>,
}

/// One model, as models.dev describes it.
#[derive(Debug, Deserialize)]
pub struct ModelRecord {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub reasoning: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub reasoning_options: Vec<ReasoningOption>,
    #[serde(default, deserialize_with = "nullable")]
    pub tool_call: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub temperature: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub structured_output: bool,
    #[serde(default)]
    pub interleaved: Option<Interleaved>,
    #[serde(default, deserialize_with = "nullable")]
    pub modalities: Modalities,
    #[serde(default, deserialize_with = "nullable")]
    pub limit: Limit,
    #[serde(default)]
    pub cost: Option<Cost>,
}

/// How a model expresses reasoning effort.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    /// Thinking can be turned off.
    Toggle,
    /// Named efforts, in the order the provider lists them.
    Effort {
        #[serde(default, deserialize_with = "string_list")]
        values: Vec<String>,
    },
    /// A token budget, which [`Model`] has no field for. Signed, because the
    /// document uses `-1` for "the provider picks".
    BudgetTokens {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    #[serde(other)]
    Unknown,
}

/// Whether reasoning is replayed inline, and under which field name.
///
/// The document writes this either as a bare `true` or as `{"field": "..."}`,
/// so both shapes are accepted.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    Named {
        #[serde(default, deserialize_with = "nullable")]
        field: String,
    },
    Enabled(bool),
}

impl Interleaved {
    /// The field name, or `""` when the document only says `true`.
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            Interleaved::Named { field } => field,
            Interleaved::Enabled(_) => "",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Modalities {
    #[serde(default, deserialize_with = "string_list")]
    pub input: Vec<String>,
    #[serde(default, deserialize_with = "string_list")]
    pub output: Vec<String>,
}

/// Token limits.
///
/// Signed and widened on the way in: this is someone else's document, and one
/// out-of-range number should not fail the parse of all six thousand models.
/// The conversion brings them back to what [`Model`] holds.
#[derive(Debug, Default, Deserialize)]
pub struct Limit {
    #[serde(default, deserialize_with = "nullable")]
    pub context: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub output: i64,
}

/// A count from the document, as a `u32` [`Model`] can hold.
fn clamp(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

#[derive(Debug, Default, Deserialize)]
pub struct Cost {
    #[serde(default, deserialize_with = "nullable")]
    pub input: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub output: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cache_read: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cache_write: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Deserialize)]
pub struct Tier {
    #[serde(default, deserialize_with = "nullable")]
    pub input: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub output: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cache_read: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub cache_write: f64,
    pub tier: TierKind,
}

#[derive(Debug, Deserialize)]
pub struct TierKind {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, deserialize_with = "nullable")]
    pub size: i64,
}

/// One model under one provider. Borrows the [`Index`] rather than copying a
/// multi-megabyte document into owned strings.
#[derive(Copy, Clone, Debug)]
pub struct Entry<'i> {
    pub provider_id: &'i str,
    pub provider: &'i Provider,
    pub model_id: &'i str,
    pub model: &'i ModelRecord,
}

impl Entry<'_> {
    /// `provider/model`, the unambiguous name `aphid model add` takes.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

impl Index {
    pub fn providers(&self) -> impl Iterator<Item = (&str, &Provider)> {
        self.providers
            .iter()
            .map(|(id, provider)| (id.as_str(), provider))
    }

    #[must_use]
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn model_count(&self) -> usize {
        self.providers.values().map(|p| p.models.len()).sum()
    }

    /// Every model under every provider.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> {
        self.providers.iter().flat_map(|(provider_id, provider)| {
            provider.models.iter().map(move |(model_id, model)| Entry {
                provider_id,
                provider,
                model_id,
                model,
            })
        })
    }

    fn entry(&self, provider_id: &str, model_id: &str) -> Option<Entry<'_>> {
        let (provider_id, provider) = self.providers.get_key_value(provider_id)?;
        let (model_id, model) = provider.models.get_key_value(model_id)?;
        Some(Entry {
            provider_id,
            provider,
            model_id,
            model,
        })
    }
}

/// Why a name did not name one model.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FindError {
    #[error("no model named `{name}` on models.dev")]
    Unknown { name: String },
    #[error("`{name}` is served by {} providers: {}", .providers.len(), .providers.join(", "))]
    Ambiguous {
        name: String,
        providers: Vec<String>,
    },
}

/// Resolve a user-supplied name to one model.
///
/// Tried in order: the whole string as a model id, then `provider/model` split
/// at the first slash. That order matters — models.dev has model ids that
/// contain a slash (`groq` serves `openai/gpt-oss-120b`), so splitting first
/// would read the model's own prefix as a provider.
///
/// # Errors
///
/// Fails when nothing matched, or when a bare id is served by several providers.
pub fn find<'i>(index: &'i Index, name: &str) -> Result<Entry<'i>, FindError> {
    let name = name.trim();

    let whole: Vec<Entry<'i>> = index
        .entries()
        .filter(|entry| entry.model_id == name)
        .collect();
    match whole.as_slice() {
        [only] => return Ok(*only),
        [] => {}
        many => {
            // Only ambiguous if the qualified form cannot save us below.
            if !name.contains('/') {
                return Err(FindError::Ambiguous {
                    name: name.to_owned(),
                    providers: many.iter().map(|e| e.provider_id.to_owned()).collect(),
                });
            }
        }
    }

    if let Some((provider_id, model_id)) = name.split_once('/')
        && let Some(entry) = index.entry(provider_id, model_id)
    {
        return Ok(entry);
    }

    match whole.as_slice() {
        [] => Err(FindError::Unknown {
            name: name.to_owned(),
        }),
        many => Err(FindError::Ambiguous {
            name: name.to_owned(),
            providers: many.iter().map(|e| e.provider_id.to_owned()).collect(),
        }),
    }
}

/// Every model whose provider id, model id or name contains `query`.
///
/// Case-insensitive, and ordered `provider/model` so the output is stable
/// between runs.
#[must_use]
pub fn search<'i>(index: &'i Index, query: &str) -> Vec<Entry<'i>> {
    let query = query.trim().to_lowercase();
    index
        .entries()
        .filter(|entry| {
            entry.provider_id.to_lowercase().contains(&query)
                || entry.model_id.to_lowercase().contains(&query)
                || entry.model.name.to_lowercase().contains(&query)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// What the user asked to override, when the document is wrong or silent.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub base_url: Option<String>,
    pub api: Option<Api>,
    pub api_key_env: Option<String>,
    pub compat: Option<CompatProfile>,
}

/// A converted model, plus anything the conversion had to gloss over.
#[derive(Clone, Debug)]
pub struct Conversion {
    pub model: Model,
    pub notes: Vec<String>,
}

/// Why a record could not become a [`Model`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    #[error(
        "{provider} speaks {npm}, and aphid only implements the OpenAI Chat Completions \
         protocol. Pass `--api openai-completions` to use it anyway."
    )]
    UnsupportedApi { provider: String, npm: String },
    #[error("models.dev lists no base URL for {provider}. Pass `--base-url <url>`.")]
    MissingBaseUrl { provider: String },
}

/// The npm packages whose providers speak OpenAI Chat Completions.
const OPENAI_COMPLETIONS_NPM: [&str; 3] = [
    "@ai-sdk/openai-compatible",
    "@ai-sdk/openai",
    "@ai-sdk/azure",
];

/// Turn a models.dev record into a model aphid can talk to.
///
/// # Errors
///
/// Fails when the provider speaks a protocol aphid does not implement, or when
/// the document lists no base URL and none was supplied.
pub fn to_model(entry: &Entry<'_>, overrides: &Overrides) -> Result<Conversion, ConvertError> {
    let mut notes = Vec::new();

    let api = match &overrides.api {
        Some(api) => api.clone(),
        None if OPENAI_COMPLETIONS_NPM.contains(&entry.provider.npm.as_str()) => {
            Api::OpenAiCompletions
        }
        None => {
            return Err(ConvertError::UnsupportedApi {
                provider: entry.provider_id.to_owned(),
                npm: entry.provider.npm.clone(),
            });
        }
    };

    let base_url = match overrides.base_url.clone().or_else(|| {
        entry
            .provider
            .api
            .clone()
            .filter(|url| !url.trim().is_empty())
    }) {
        Some(url) => url,
        None => {
            return Err(ConvertError::MissingBaseUrl {
                provider: entry.provider_id.to_owned(),
            });
        }
    };

    let api_key_env = overrides
        .api_key_env
        .clone()
        .or_else(|| entry.provider.env.first().cloned());
    if api_key_env.is_none() {
        notes.push(format!(
            "models.dev names no API key variable for {}; set `api_key_env` by hand if it needs one",
            entry.provider_id
        ));
    }

    let profile = overrides.compat.unwrap_or_else(|| default_profile(entry));
    let compat = refine_compat(profile.to_compat(&api), entry);

    let mut input = InputModalities::default();
    for modality in &entry.model.modalities.input {
        match modality.as_str() {
            "text" => input = input | InputModalities::TEXT,
            "image" => input = input | InputModalities::IMAGE,
            other => notes.push(format!("ignoring the `{other}` input modality")),
        }
    }
    if input.is_empty() {
        input = InputModalities::TEXT;
    }

    let thinking_levels = thinking_levels(entry, &compat, &mut notes);

    if clamp(entry.model.limit.context) == 0 {
        notes.push("models.dev lists no context window; set `context_window` by hand".to_owned());
    }
    if clamp(entry.model.limit.output) == 0 {
        notes.push("models.dev lists no output limit; set `max_tokens` by hand".to_owned());
    }
    if entry.model.cost.is_none() {
        notes.push("models.dev lists no price, so usage will report $0".to_owned());
    }

    let model = Model {
        id: CompactString::new(entry.model_id),
        name: if entry.model.name.is_empty() {
            entry.model_id.to_owned()
        } else {
            entry.model.name.clone()
        },
        api,
        provider: ProviderId::new(entry.provider_id),
        base_url,
        api_key_env: api_key_env.map(CompactString::new),
        reasoning: entry.model.reasoning,
        thinking_levels,
        input,
        cost: cost(entry.model.cost.as_ref()),
        context_window: clamp(entry.model.limit.context),
        max_tokens: clamp(entry.model.limit.output),
        sampling_params: None,
        headers: Vec::new(),
        compat,
    };

    Ok(Conversion { model, notes })
}

/// Which quirks table to start from.
///
/// Keyed off the provider, because that is what the quirks belong to. `deepseek`
/// has a table aphid has verified; a provider that merely claims the protocol
/// gets the conservative [`OpenAiCompletionsCompat::compatible`] one; OpenAI and
/// Azure get OpenAI's own.
///
/// [`OpenAiCompletionsCompat::compatible`]: crate::OpenAiCompletionsCompat::compatible
fn default_profile(entry: &Entry<'_>) -> CompatProfile {
    if entry.provider_id == ProviderId::DEEPSEEK.as_str() {
        return CompatProfile::Deepseek;
    }
    match entry.provider.npm.as_str() {
        "@ai-sdk/openai" | "@ai-sdk/azure" => CompatProfile::Openai,
        _ => CompatProfile::Compatible,
    }
}

/// Apply the two things the record genuinely says about the endpoint.
fn refine_compat(compat: Compat, entry: &Entry<'_>) -> Compat {
    let Some(flags) = compat.openai_completions() else {
        return compat;
    };
    let mut flags = flags.clone();

    flags.supports_reasoning_effort = entry
        .model
        .reasoning_options
        .iter()
        .any(|option| matches!(option, ReasoningOption::Effort { .. }));
    flags.requires_reasoning_content_on_assistant_messages = entry
        .model
        .interleaved
        .as_ref()
        .is_some_and(|interleaved| interleaved.field() == "reasoning_content");

    Compat::from(flags)
}

/// aphid's six-level ladder, lowest first.
const LADDER: [ThinkingLevel; 6] = [
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];

/// Fold the efforts a model offers onto aphid's ladder.
///
/// Each aphid level takes the lowest offered effort that is at least as strong,
/// and the strongest on offer when there is nothing above. Rounding up is what
/// the hand-written DeepSeek map does — `medium` is sent as `high`, because
/// DeepSeek offers no middle — and asking for at least as much reasoning as the
/// user wanted is the safer direction to be wrong in.
fn thinking_levels(
    entry: &Entry<'_>,
    compat: &Compat,
    notes: &mut Vec<String>,
) -> ThinkingLevelMap {
    let mut map = ThinkingLevelMap::all_default();

    if !entry.model.reasoning {
        for level in LADDER {
            map.set(level.into(), LevelMapping::Unsupported);
        }
        map.set(ModelThinkingLevel::Off, LevelMapping::Unsupported);
        return map;
    }

    let mut offered: Vec<&str> = Vec::new();
    let mut off_value: Option<&str> = None;
    let mut toggle = false;
    for option in &entry.model.reasoning_options {
        match option {
            ReasoningOption::Toggle => toggle = true,
            ReasoningOption::Effort { values } => {
                for value in values {
                    // `none` is how a provider spells "do not think", not a
                    // rung on the ladder.
                    if value == "none" || value == "off" || value == "disabled" {
                        off_value = Some(value.as_str());
                    } else if rank(value).is_some() {
                        offered.push(value.as_str());
                    } else {
                        notes.push(format!("ignoring the unknown reasoning effort `{value}`"));
                    }
                }
            }
            ReasoningOption::BudgetTokens { .. } => {
                notes.push(
                    "this model grades reasoning by token budget, which aphid sends as a named \
                     effort; check `thinking_levels` in models.json"
                        .to_owned(),
                );
            }
            ReasoningOption::Unknown => {}
        }
    }

    offered.sort_by_key(|value| rank(value).unwrap_or(0));
    offered.dedup();

    if offered.is_empty() {
        // Reasoning with no published efforts: send aphid's own names and let
        // the endpoint decide.
        notes.push(
            "models.dev lists no reasoning efforts for this model, so aphid sends its own level \
             names"
                .to_owned(),
        );
    } else {
        for level in LADDER {
            let wanted = rank(level.as_str()).unwrap_or(0);
            let chosen = offered
                .iter()
                .find(|value| rank(value).unwrap_or(0) >= wanted)
                .or_else(|| offered.last())
                .expect("a non-empty offer list");
            map.set(
                level.into(),
                LevelMapping::Value(CompactString::new(chosen)),
            );
        }
    }

    let off = match off_value {
        Some(value) => LevelMapping::Value(CompactString::new(value)),
        None if toggle => match compat.openai_completions().map(|c| c.thinking_format) {
            Some(crate::compat::ThinkingFormat::DeepSeek) => {
                LevelMapping::Value(CompactString::const_new("disabled"))
            }
            _ => LevelMapping::Default,
        },
        None => LevelMapping::Unsupported,
    };
    map.set(ModelThinkingLevel::Off, off);

    map
}

/// Where a named effort sits on aphid's ladder.
fn rank(value: &str) -> Option<usize> {
    LADDER
        .iter()
        .position(|level| level.as_str() == value)
        .map(|at| at + 1)
}

fn cost(cost: Option<&Cost>) -> ModelCost {
    let Some(cost) = cost else {
        return ModelCost::default();
    };
    ModelCost {
        rates: ModelCostRates {
            input: cost.input,
            output: cost.output,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        },
        tiers: cost
            .tiers
            .iter()
            // `context` is the only tier kind the document uses, and the only
            // one `ModelCost::rates_for` knows how to apply.
            .filter(|tier| tier.tier.kind == "context")
            .map(|tier| ModelCostTier {
                input_tokens_above: clamp(tier.tier.size),
                rates: ModelCostRates {
                    input: tier.input,
                    output: tier.output,
                    cache_read: tier.cache_read,
                    cache_write: tier.cache_write,
                },
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Fetching and caching
// ---------------------------------------------------------------------------

/// Why the document could not be obtained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not reach models.dev: {0}")]
    Http(#[from] reqwest::Error),
    #[error("models.dev answered {status}")]
    Status { status: u16 },
    #[error("models.dev sent something that is not the expected document: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("could not write the cache at {path}: {source}")]
    Cache {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "no cached copy of models.dev, and --offline forbids fetching one. Run `aphid model update`."
    )]
    Offline,
}

/// A cached copy, and how old it is.
#[derive(Clone, Debug)]
pub struct Cached {
    pub body: String,
    pub age: Duration,
}

impl Cached {
    #[must_use]
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age <= ttl
    }
}

/// When to go to the network.
#[derive(Copy, Clone, Debug)]
pub enum CachePolicy {
    /// Use the cache while it is younger than `ttl`.
    Ttl(Duration),
    /// Fetch regardless, and rewrite the cache.
    Refresh,
    /// Never fetch. Fails when there is no cache at all.
    Offline,
}

impl Default for CachePolicy {
    fn default() -> Self {
        CachePolicy::Ttl(DEFAULT_TTL)
    }
}

/// Where the document that was used came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Cache { age: Duration, stale: bool },
    Network,
}

/// Download the document. Returns the body, unparsed, so the cache can store
/// exactly what the server sent.
///
/// # Errors
///
/// Fails on a transport error or a non-success status.
pub async fn fetch() -> Result<String, Error> {
    let response = crate::api::client().get(API_URL).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Status {
            status: status.as_u16(),
        });
    }
    Ok(response.text().await?)
}

/// Parse a document body.
///
/// # Errors
///
/// Fails when the body is not the document models.dev publishes.
pub fn parse(body: &str) -> Result<Index, Error> {
    Ok(serde_json::from_str(body)?)
}

/// Read the cached document, if there is one.
///
/// A file whose modification time is in the future reads as age zero rather than
/// as an error: a bad clock should not make aphid refetch forever.
#[must_use]
pub fn read_cache(path: &Path) -> Option<Cached> {
    let body = std::fs::read_to_string(path).ok()?;
    let age = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified.elapsed().unwrap_or_default())
        .unwrap_or_default();
    Some(Cached { body, age })
}

/// Write the document to the cache, through a temporary file and a rename.
///
/// # Errors
///
/// Fails when the directory cannot be created or the file cannot be written.
pub fn write_cache(path: &Path, body: &str) -> std::io::Result<()> {
    catalog::write_atomically(path, body)
}

/// Get the document, from the cache or the network, per `policy`.
///
/// A network failure with any cached copy present falls back to that copy and
/// reports it as stale, because a day-old price is far more useful than an
/// error.
///
/// # Errors
///
/// Fails when the document cannot be obtained at all, or cannot be parsed.
pub async fn load(path: &Path, policy: CachePolicy) -> Result<(Index, Source), Error> {
    let cached = read_cache(path);

    if let CachePolicy::Ttl(ttl) = policy
        && let Some(cached) = &cached
        && cached.is_fresh(ttl)
    {
        return Ok((
            parse(&cached.body)?,
            Source::Cache {
                age: cached.age,
                stale: false,
            },
        ));
    }

    if let CachePolicy::Offline = policy {
        let cached = cached.ok_or(Error::Offline)?;
        return Ok((
            parse(&cached.body)?,
            Source::Cache {
                age: cached.age,
                stale: true,
            },
        ));
    }

    match fetch().await {
        Ok(body) => {
            let index = parse(&body)?;
            write_cache(path, &body).map_err(|source| Error::Cache {
                path: path.to_path_buf(),
                source,
            })?;
            Ok((index, Source::Network))
        }
        Err(error) => match cached {
            Some(cached) => Ok((
                parse(&cached.body)?,
                Source::Cache {
                    age: cached.age,
                    stale: true,
                },
            )),
            None => Err(error),
        },
    }
}
