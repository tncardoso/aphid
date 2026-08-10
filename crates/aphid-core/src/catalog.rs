//! The user's model catalog: `~/.aphid/models.json`.
//!
//! [`providers`](crate::providers) writes model metadata out as constants, which
//! is right for the handful aphid ships. This is the other half: a file the user
//! owns, so a model aphid has never heard of can be described once and then
//! selected like any built-in.
//!
//! The on-disk shape is deliberately *not* [`Model`] with `derive(Serialize)`.
//! `Model` is a runtime layout — `ThinkingLevelMap` is an array indexed by level,
//! `Compat` is a flag soup — and neither makes a file anyone would want to edit.
//! [`ModelEntry`] is the editable projection: defaults are omitted, thinking
//! levels are named, and the compatibility profile is a name plus whichever
//! flags differ from it.
//!
//! Paths are parameters here, not environment lookups, so tests never have to
//! move `$HOME`. [`config_path`] is the one function that reads the environment,
//! and it is meant to be called once at the edge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::compat::{Compat, MaxTokensField, OpenAiCompletionsCompat, ThinkingFormat};
use crate::json::Json;
use crate::model::{InputModalities, Model, ModelCost, ModelCostRates, ModelCostTier};
use crate::provider::{Api, ProviderId};
use crate::thinking::{LevelMapping, ModelThinkingLevel, ThinkingLevel, ThinkingLevelMap};

/// The directory everything user-level lives in, under the home directory.
pub const DIR_NAME: &str = ".aphid";
/// Overrides the whole directory, mostly so tests and scripts can sandbox it.
pub const HOME_ENV: &str = "APHID_HOME";
/// The catalog file, inside [`DIR_NAME`].
pub const MODELS_FILE: &str = "models.json";
/// The cached models.dev document, inside [`DIR_NAME`]. See [`crate::models_dev`].
pub const CACHE_FILE: &str = "models.dev.json";
/// The format version written by this build.
pub const VERSION: u32 = 1;

/// `$APHID_HOME`, or `~/.aphid`.
///
/// `None` when there is no home directory to speak of, which is a real
/// possibility in a container and not worth panicking over.
#[must_use]
pub fn aphid_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(HOME_ENV)
        && !explicit.is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(DIR_NAME))
}

/// Where [`load`] and [`save`] look by default.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    aphid_dir().map(|dir| dir.join(MODELS_FILE))
}

/// Where the models.dev cache lives.
#[must_use]
pub fn cache_path() -> Option<PathBuf> {
    aphid_dir().map(|dir| dir.join(CACHE_FILE))
}

/// Why a catalog file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path}: version {found} was written by a newer aphid; this one understands {VERSION}")]
    Version { path: PathBuf, found: u32 },
}

/// Why one entry could not become a [`Model`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EntryError {
    #[error("a model needs an `id`")]
    MissingId,
    #[error("{id}: a model needs a `base_url`")]
    MissingBaseUrl { id: String },
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub version: u32,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            version: VERSION,
            models: Vec::new(),
        }
    }
}

impl ModelsConfig {
    #[must_use]
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|entry| entry.id == id)
    }

    /// Add an entry, replacing any with the same id. `true` when one was
    /// replaced.
    pub fn push_or_replace(&mut self, entry: ModelEntry) -> bool {
        match self.models.iter_mut().find(|held| held.id == entry.id) {
            Some(held) => {
                *held = entry;
                true
            }
            None => {
                self.models.push(entry);
                false
            }
        }
    }

    /// Drop the entry with this exact id.
    pub fn remove(&mut self, id: &str) -> Option<ModelEntry> {
        let at = self.models.iter().position(|entry| entry.id == id)?;
        Some(self.models.remove(at))
    }
}

/// Read the catalog. A missing file is an empty catalog, not a failure.
///
/// # Errors
///
/// Fails when the file exists but cannot be read or parsed, or when it was
/// written by a newer aphid.
pub fn load(path: &Path) -> Result<ModelsConfig, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelsConfig::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    // An empty file is what a truncated write leaves behind. Treat it the same
    // as no file rather than reporting a parse error nobody can act on.
    if text.trim().is_empty() {
        return Ok(ModelsConfig::default());
    }

    let config: ModelsConfig =
        serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if config.version > VERSION {
        return Err(ConfigError::Version {
            path: path.to_path_buf(),
            found: config.version,
        });
    }
    Ok(config)
}

/// Write the catalog, creating the directory if it is not there.
///
/// Goes through a temporary sibling and a rename, so a process killed mid-write
/// cannot leave a half-written catalog that then fails to load.
///
/// # Errors
///
/// Fails when the directory cannot be created or the file cannot be written.
pub fn save(path: &Path, config: &ModelsConfig) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(config)?;
    text.push('\n');
    write_atomically(path, &text)
}

/// Write `contents` to `path` through a temporary sibling and a rename.
///
/// Shared with [`crate::models_dev`], which caches a multi-megabyte document and
/// wants the same guarantee.
///
/// # Errors
///
/// Fails when the parent directory cannot be created, or the write or rename
/// fails.
pub fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);

    std::fs::write(&temporary, contents)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// One model, as written in the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The id sent on the wire, and what `--model` matches against.
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub provider: String,
    /// The wire protocol. Only `openai-completions` is implemented today.
    #[serde(default = "default_api")]
    pub api: String,
    pub base_url: String,
    /// Environment variable holding the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
    /// Per-level wire values. Absent levels use their own name; see
    /// [`ThinkingLevels`].
    #[serde(default, skip_serializing_if = "ThinkingLevels::is_empty")]
    pub thinking_levels: ThinkingLevels,
    /// `text`, `image`. Anything else is ignored rather than refused.
    #[serde(default = "default_input")]
    pub input: Vec<String>,
    pub context_window: u32,
    pub max_tokens: u32,
    #[serde(default)]
    pub cost: CostEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Json>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "CompatEntry::is_default")]
    pub compat: CompatEntry,
}

fn default_api() -> String {
    Api::OpenAiCompletions.as_str().to_owned()
}

fn default_input() -> Vec<String> {
    vec!["text".to_owned()]
}

impl TryFrom<&ModelEntry> for Model {
    type Error = EntryError;

    fn try_from(entry: &ModelEntry) -> Result<Self, Self::Error> {
        if entry.id.trim().is_empty() {
            return Err(EntryError::MissingId);
        }
        if entry.base_url.trim().is_empty() {
            return Err(EntryError::MissingBaseUrl {
                id: entry.id.clone(),
            });
        }

        let mut input = InputModalities::default();
        for modality in &entry.input {
            match modality.as_str() {
                "text" => input = input | InputModalities::TEXT,
                "image" => input = input | InputModalities::IMAGE,
                _ => {}
            }
        }

        let api: Api = entry.api.parse().unwrap_or(Api::OpenAiCompletions);
        Ok(Model {
            id: CompactString::new(&entry.id),
            name: if entry.name.is_empty() {
                entry.id.clone()
            } else {
                entry.name.clone()
            },
            compat: entry.compat.to_compat(&api),
            api,
            provider: ProviderId::new(entry.provider.as_str()),
            base_url: entry.base_url.clone(),
            api_key_env: entry
                .api_key_env
                .as_deref()
                .filter(|env| !env.is_empty())
                .map(CompactString::new),
            reasoning: entry.reasoning,
            thinking_levels: entry.thinking_levels.to_map(entry.reasoning),
            input,
            cost: entry.cost.to_cost(),
            context_window: entry.context_window,
            max_tokens: entry.max_tokens,
            sampling_params: entry.sampling_params.clone(),
            headers: entry
                .headers
                .iter()
                .map(|(name, value)| (CompactString::new(name), value.clone()))
                .collect(),
        })
    }
}

impl From<&Model> for ModelEntry {
    fn from(model: &Model) -> Self {
        let mut input = Vec::new();
        if model.input.contains(InputModalities::TEXT) {
            input.push("text".to_owned());
        }
        if model.input.contains(InputModalities::IMAGE) {
            input.push("image".to_owned());
        }

        ModelEntry {
            id: model.id.to_string(),
            name: model.name.clone(),
            provider: model.provider.to_string(),
            api: model.api.as_str().to_owned(),
            base_url: model.base_url.clone(),
            api_key_env: model.api_key_env.as_ref().map(ToString::to_string),
            reasoning: model.reasoning,
            thinking_levels: ThinkingLevels::from_map(&model.thinking_levels, model.reasoning),
            input,
            context_window: model.context_window,
            max_tokens: model.max_tokens,
            cost: CostEntry::from_cost(&model.cost),
            sampling_params: model.sampling_params.clone(),
            headers: model
                .headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
            compat: CompatEntry::from_compat(&model.compat),
        }
    }
}

/// What a model does with one thinking level.
///
/// `false` is "the model rejects this level"; a string is the value to put on
/// the wire. Leaving the level out means "send its own name", which is why the
/// common case writes nothing at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LevelEntry {
    Supported(bool),
    Value(String),
}

/// Per-level wire values, in ladder order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingLevels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xhigh: Option<LevelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<LevelEntry>,
}

/// The seven levels, in the order they are written.
const LADDER: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Level(ThinkingLevel::Minimal),
    ModelThinkingLevel::Level(ThinkingLevel::Low),
    ModelThinkingLevel::Level(ThinkingLevel::Medium),
    ModelThinkingLevel::Level(ThinkingLevel::High),
    ModelThinkingLevel::Level(ThinkingLevel::XHigh),
    ModelThinkingLevel::Level(ThinkingLevel::Max),
];

impl ThinkingLevels {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        LADDER.iter().all(|level| self.get(*level).is_none())
    }

    fn get(&self, level: ModelThinkingLevel) -> Option<&LevelEntry> {
        match level {
            ModelThinkingLevel::Off => self.off.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::Minimal) => self.minimal.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::Low) => self.low.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::Medium) => self.medium.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::High) => self.high.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::XHigh) => self.xhigh.as_ref(),
            ModelThinkingLevel::Level(ThinkingLevel::Max) => self.max.as_ref(),
        }
    }

    fn set(&mut self, level: ModelThinkingLevel, entry: Option<LevelEntry>) {
        let slot = match level {
            ModelThinkingLevel::Off => &mut self.off,
            ModelThinkingLevel::Level(ThinkingLevel::Minimal) => &mut self.minimal,
            ModelThinkingLevel::Level(ThinkingLevel::Low) => &mut self.low,
            ModelThinkingLevel::Level(ThinkingLevel::Medium) => &mut self.medium,
            ModelThinkingLevel::Level(ThinkingLevel::High) => &mut self.high,
            ModelThinkingLevel::Level(ThinkingLevel::XHigh) => &mut self.xhigh,
            ModelThinkingLevel::Level(ThinkingLevel::Max) => &mut self.max,
        };
        *slot = entry;
    }

    /// Expand into the runtime map.
    ///
    /// A model that does not reason starts from "every level unsupported", so
    /// the common non-reasoning entry writes no thinking block at all.
    #[must_use]
    pub fn to_map(&self, reasoning: bool) -> ThinkingLevelMap {
        let base = if reasoning {
            LevelMapping::Default
        } else {
            LevelMapping::Unsupported
        };
        let mut map = ThinkingLevelMap::all_default();
        for level in LADDER {
            let mapping = match self.get(level) {
                None => base.clone(),
                Some(LevelEntry::Supported(true)) => LevelMapping::Default,
                Some(LevelEntry::Supported(false)) => LevelMapping::Unsupported,
                Some(LevelEntry::Value(value)) => LevelMapping::Value(CompactString::new(value)),
            };
            map.set(level, mapping);
        }
        map
    }

    /// Project a runtime map back onto the file, writing only what differs from
    /// what [`to_map`](Self::to_map) would rebuild.
    #[must_use]
    pub fn from_map(map: &ThinkingLevelMap, reasoning: bool) -> Self {
        let mut levels = Self::default();
        for level in LADDER {
            let entry = match map.get(level) {
                LevelMapping::Default if reasoning => None,
                LevelMapping::Default => Some(LevelEntry::Supported(true)),
                LevelMapping::Unsupported if reasoning => Some(LevelEntry::Supported(false)),
                LevelMapping::Unsupported => None,
                LevelMapping::Value(value) => Some(LevelEntry::Value(value.to_string())),
            };
            levels.set(level, entry);
        }
        levels
    }
}

/// Price per million tokens, plus any context tiers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostEntry {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierEntry>,
}

/// A price that takes over above an input size.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TierEntry {
    pub input_tokens_above: u32,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

impl CostEntry {
    #[must_use]
    pub fn to_cost(&self) -> ModelCost {
        ModelCost {
            rates: ModelCostRates {
                input: self.input,
                output: self.output,
                cache_read: self.cache_read,
                cache_write: self.cache_write,
            },
            tiers: self
                .tiers
                .iter()
                .map(|tier| ModelCostTier {
                    input_tokens_above: tier.input_tokens_above,
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

    #[must_use]
    pub fn from_cost(cost: &ModelCost) -> Self {
        Self {
            input: cost.rates.input,
            output: cost.rates.output,
            cache_read: cost.rates.cache_read,
            cache_write: cost.rates.cache_write,
            tiers: cost
                .tiers
                .iter()
                .map(|tier| TierEntry {
                    input_tokens_above: tier.input_tokens_above,
                    input: tier.rates.input,
                    output: tier.rates.output,
                    cache_read: tier.rates.cache_read,
                    cache_write: tier.rates.cache_write,
                })
                .collect(),
        }
    }
}

/// Which set of endpoint quirks to start from.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatProfile {
    /// OpenAI's own behaviour, [`OpenAiCompletionsCompat::default`].
    #[default]
    Openai,
    /// A generic third-party endpoint,
    /// [`OpenAiCompletionsCompat::compatible`]. What `aphid model add` picks
    /// for a provider that only claims to speak the protocol.
    Compatible,
    /// [`OpenAiCompletionsCompat::deepseek`].
    Deepseek,
    /// No quirks table at all.
    None,
}

impl CompatProfile {
    /// The flags this profile names, before any overrides.
    ///
    /// `None` when the profile is [`CompatProfile::None`], or when the API
    /// family is not OpenAI Chat Completions and so has no quirks table.
    #[must_use]
    pub fn flags(self, api: &Api) -> Option<OpenAiCompletionsCompat> {
        if self == CompatProfile::None || !matches!(api, Api::OpenAiCompletions) {
            return None;
        }
        Some(match self {
            CompatProfile::Deepseek => OpenAiCompletionsCompat::deepseek(),
            CompatProfile::Compatible => OpenAiCompletionsCompat::compatible(),
            CompatProfile::Openai | CompatProfile::None => OpenAiCompletionsCompat::default(),
        })
    }

    /// The profile as a [`Compat`], with nothing overridden.
    #[must_use]
    pub fn to_compat(self, api: &Api) -> Compat {
        self.flags(api).map_or(Compat::None, Compat::from)
    }
}

/// A compatibility profile, plus whichever flags differ from it.
///
/// Written this way so an entry says only what is unusual about the endpoint —
/// the alternative is fifteen booleans per model, which nobody would read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatEntry {
    #[serde(default)]
    pub profile: CompatProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature_while_thinking: Option<bool>,
    /// `max_completion_tokens` or `max_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    /// `openai` or `deepseek`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<String>,
}

impl CompatEntry {
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The flags this entry describes.
    #[must_use]
    pub fn to_compat(&self, api: &Api) -> Compat {
        // A custom API family, like `None`, has no quirks table to fill in.
        let Some(mut compat) = self.profile.flags(api) else {
            return Compat::None;
        };

        apply(&mut compat.supports_store, self.supports_store);
        apply(
            &mut compat.supports_developer_role,
            self.supports_developer_role,
        );
        apply(
            &mut compat.supports_reasoning_effort,
            self.supports_reasoning_effort,
        );
        apply(
            &mut compat.supports_usage_in_streaming,
            self.supports_usage_in_streaming,
        );
        apply(
            &mut compat.supports_finish_reason,
            self.supports_finish_reason,
        );
        apply(&mut compat.supports_strict_mode, self.supports_strict_mode);
        apply(
            &mut compat.supports_long_cache_retention,
            self.supports_long_cache_retention,
        );
        apply(
            &mut compat.requires_tool_result_name,
            self.requires_tool_result_name,
        );
        apply(
            &mut compat.requires_assistant_after_tool_result,
            self.requires_assistant_after_tool_result,
        );
        apply(
            &mut compat.requires_thinking_as_text,
            self.requires_thinking_as_text,
        );
        apply(
            &mut compat.requires_reasoning_content_on_assistant_messages,
            self.requires_reasoning_content_on_assistant_messages,
        );
        apply(
            &mut compat.supports_temperature_while_thinking,
            self.supports_temperature_while_thinking,
        );

        if let Some(field) = self.max_tokens_field.as_deref() {
            compat.max_tokens_field = match field {
                "max_tokens" => MaxTokensField::MaxTokens,
                _ => MaxTokensField::MaxCompletionTokens,
            };
        }
        if let Some(format) = self.thinking_format.as_deref() {
            compat.thinking_format = match format {
                "deepseek" => ThinkingFormat::DeepSeek,
                _ => ThinkingFormat::OpenAi,
            };
        }

        Compat::from(compat)
    }

    /// Pick the nearest profile and record only what it does not already say.
    ///
    /// "Nearest" is measured, not assumed: each built-in profile is scored by
    /// how many flags it gets wrong, and the best one wins. An exact match
    /// writes nothing but its name, which is the common case and keeps the file
    /// to one line per model.
    #[must_use]
    pub fn from_compat(compat: &Compat) -> Self {
        let Some(flags) = compat.openai_completions() else {
            return Self {
                profile: CompatProfile::None,
                ..Self::default()
            };
        };

        [
            CompatProfile::Openai,
            CompatProfile::Compatible,
            CompatProfile::Deepseek,
        ]
        .into_iter()
        .map(|profile| Self::against(profile, flags))
        .min_by_key(Self::overrides)
        .unwrap_or_default()
    }

    /// This profile, plus every flag it does not already get right.
    fn against(profile: CompatProfile, flags: &OpenAiCompletionsCompat) -> Self {
        let base = profile.flags(&Api::OpenAiCompletions).unwrap_or_default();
        Self {
            profile,
            supports_store: differing(flags.supports_store, base.supports_store),
            supports_developer_role: differing(
                flags.supports_developer_role,
                base.supports_developer_role,
            ),
            supports_reasoning_effort: differing(
                flags.supports_reasoning_effort,
                base.supports_reasoning_effort,
            ),
            supports_usage_in_streaming: differing(
                flags.supports_usage_in_streaming,
                base.supports_usage_in_streaming,
            ),
            supports_finish_reason: differing(
                flags.supports_finish_reason,
                base.supports_finish_reason,
            ),
            supports_strict_mode: differing(flags.supports_strict_mode, base.supports_strict_mode),
            supports_long_cache_retention: differing(
                flags.supports_long_cache_retention,
                base.supports_long_cache_retention,
            ),
            requires_tool_result_name: differing(
                flags.requires_tool_result_name,
                base.requires_tool_result_name,
            ),
            requires_assistant_after_tool_result: differing(
                flags.requires_assistant_after_tool_result,
                base.requires_assistant_after_tool_result,
            ),
            requires_thinking_as_text: differing(
                flags.requires_thinking_as_text,
                base.requires_thinking_as_text,
            ),
            requires_reasoning_content_on_assistant_messages: differing(
                flags.requires_reasoning_content_on_assistant_messages,
                base.requires_reasoning_content_on_assistant_messages,
            ),
            supports_temperature_while_thinking: differing(
                flags.supports_temperature_while_thinking,
                base.supports_temperature_while_thinking,
            ),
            max_tokens_field: (flags.max_tokens_field != base.max_tokens_field)
                .then(|| flags.max_tokens_field.as_str().to_owned()),
            thinking_format: (flags.thinking_format != base.thinking_format).then(|| {
                match flags.thinking_format {
                    ThinkingFormat::DeepSeek => "deepseek".to_owned(),
                    ThinkingFormat::OpenAi => "openai".to_owned(),
                }
            }),
        }
    }

    /// How many flags this entry has to state on top of its profile.
    fn overrides(&self) -> usize {
        [
            self.supports_store,
            self.supports_developer_role,
            self.supports_reasoning_effort,
            self.supports_usage_in_streaming,
            self.supports_finish_reason,
            self.supports_strict_mode,
            self.supports_long_cache_retention,
            self.requires_tool_result_name,
            self.requires_assistant_after_tool_result,
            self.requires_thinking_as_text,
            self.requires_reasoning_content_on_assistant_messages,
            self.supports_temperature_while_thinking,
        ]
        .iter()
        .filter(|flag| flag.is_some())
        .count()
            + usize::from(self.max_tokens_field.is_some())
            + usize::from(self.thinking_format.is_some())
    }
}

fn apply(flag: &mut bool, override_with: Option<bool>) {
    if let Some(value) = override_with {
        *flag = value;
    }
}

fn differing(value: bool, base: bool) -> Option<bool> {
    (value != base).then_some(value)
}
