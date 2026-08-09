//! DeepSeek.
//!
//! DeepSeek speaks the OpenAI Chat Completions protocol with a handful of
//! documented deviations, captured in
//! [`OpenAiCompletionsCompat::deepseek`](crate::OpenAiCompletionsCompat::deepseek).
//!
//! Thinking is enabled with `thinking: { "type": "enabled" | "disabled" }` and
//! graded with `reasoning_effort`, which accepts only `low`, `high` and `max` —
//! hence the mapping below folds aphid's six-level ladder onto those three.
//! Thinking mode also rejects `temperature`, `top_p`, `presence_penalty` and
//! `frequency_penalty`.
//!
//! Prices are per million tokens, from DeepSeek's published pricing. DeepSeek
//! charges nothing to write a cache entry.

use compact_str::CompactString;

use crate::compat::{Compat, OpenAiCompletionsCompat};
use crate::model::{InputModalities, Model, ModelCost};
use crate::provider::{Api, ProviderId};
use crate::thinking::{LevelMapping, ModelThinkingLevel, ThinkingLevel, ThinkingLevelMap};

pub const BASE_URL: &str = "https://api.deepseek.com";
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Both current models take a one-million-token context.
pub const CONTEXT_WINDOW: u32 = 1_000_000;
/// Both current models cap output at 384k tokens.
pub const MAX_OUTPUT_TOKENS: u32 = 384_000;

/// `deepseek-v4-flash`.
#[must_use]
pub fn flash() -> Model {
    model(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        ModelCost::flat(0.14, 0.28, 0.0028, 0.0),
    )
}

/// `deepseek-v4-pro`.
#[must_use]
pub fn pro() -> Model {
    model(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        ModelCost::flat(0.435, 0.87, 0.003_625, 0.0),
    )
}

/// Every model DeepSeek currently serves.
#[must_use]
pub fn models() -> Vec<Model> {
    vec![flash(), pro()]
}

/// aphid's six thinking levels folded onto DeepSeek's `low` / `high` / `max`.
#[must_use]
pub fn thinking_levels() -> ThinkingLevelMap {
    use ThinkingLevel::{High, Low, Max, Medium, Minimal, XHigh};
    ThinkingLevelMap::all_default()
        .with(
            Minimal.into(),
            LevelMapping::Value(CompactString::const_new("low")),
        )
        .with(
            Low.into(),
            LevelMapping::Value(CompactString::const_new("low")),
        )
        .with(
            Medium.into(),
            LevelMapping::Value(CompactString::const_new("high")),
        )
        .with(
            High.into(),
            LevelMapping::Value(CompactString::const_new("high")),
        )
        .with(
            XHigh.into(),
            LevelMapping::Value(CompactString::const_new("max")),
        )
        .with(
            Max.into(),
            LevelMapping::Value(CompactString::const_new("max")),
        )
        .with(
            ModelThinkingLevel::Off,
            LevelMapping::Value(CompactString::const_new("disabled")),
        )
}

fn model(id: &str, name: &str, cost: ModelCost) -> Model {
    Model {
        id: CompactString::new(id),
        name: name.to_owned(),
        api: Api::OpenAiCompletions,
        provider: ProviderId::DEEPSEEK,
        base_url: BASE_URL.to_owned(),
        reasoning: true,
        thinking_levels: thinking_levels(),
        input: InputModalities::TEXT,
        cost,
        context_window: CONTEXT_WINDOW,
        max_tokens: MAX_OUTPUT_TOKENS,
        sampling_params: None,
        headers: Vec::new(),
        compat: Compat::from(OpenAiCompletionsCompat::deepseek()),
    }
}
