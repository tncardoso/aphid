//! Per-request knobs.

use std::time::Duration;

use compact_str::CompactString;

use crate::json::Json;
use crate::thinking::{ThinkingBudgets, ThinkingLevel};

/// How long a provider should hold a prompt cache entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum CacheRetention {
    /// Do not request caching.
    None,
    /// The provider's default window.
    #[default]
    Short,
    /// The provider's extended window, where supported.
    Long,
}

/// Transport and authentication settings shared by every request.
#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub api_key: Option<CompactString>,
    /// Extra headers. A `None` value suppresses a default header of that name.
    pub headers: Vec<(CompactString, Option<String>)>,
    /// Provider-scoped environment overrides, taking precedence over the
    /// process environment.
    pub env: Vec<(CompactString, CompactString)>,
    pub timeout: Option<Duration>,
    pub max_retries: Option<u8>,
    /// Give up rather than honour a server-requested retry delay longer than
    /// this, so a long wait surfaces to the caller instead of hanging.
    pub max_retry_delay: Option<Duration>,
}

/// Options for a raw streaming request.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    pub request: RequestOptions,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Extra sampling parameters merged into the request body verbatim, for
    /// knobs aphid does not model (`top_p`, `min_p`, `repetition_penalty`).
    pub sampling_params: Option<Json>,
    pub cache_retention: CacheRetention,
    /// Session key for providers that route on cache affinity.
    pub session_id: Option<CompactString>,
    /// Extra request metadata; providers take the fields they understand.
    pub metadata: Option<Json>,
}

/// Options for the ergonomic entry point, which resolves reasoning settings
/// into whatever the target endpoint expects.
#[derive(Clone, Debug, Default)]
pub struct SimpleStreamOptions {
    pub stream: StreamOptions,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: ThinkingBudgets,
}
