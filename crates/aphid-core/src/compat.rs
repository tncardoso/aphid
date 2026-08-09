//! Per-endpoint quirks of the OpenAI Chat Completions protocol.
//!
//! Every "OpenAI-compatible" endpoint is compatible in a slightly different
//! way. These flags are ported from pi, which learned them the hard way; the
//! difference is that aphid states a profile explicitly at the provider rather
//! than sniffing it out of the base URL at request time, which is both faster
//! and far easier to debug.

/// Which request field caps the response length.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum MaxTokensField {
    /// The current OpenAI field.
    #[default]
    MaxCompletionTokens,
    /// The legacy field, which many compatible endpoints still require.
    MaxTokens,
}

impl MaxTokensField {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            MaxTokensField::MaxTokens => "max_tokens",
        }
    }
}

/// How reasoning effort is expressed in the request body.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ThinkingFormat {
    /// Bare `reasoning_effort`.
    #[default]
    OpenAi,
    /// `thinking: { type: "enabled" | "disabled" }`, plus `reasoning_effort`
    /// when the endpoint accepts it.
    DeepSeek,
}

/// The compatibility profile of one OpenAI-compatible endpoint.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpenAiCompletionsCompat {
    /// Endpoint accepts the `store` field.
    pub supports_store: bool,
    /// Endpoint accepts the `developer` role in place of `system`.
    pub supports_developer_role: bool,
    /// Endpoint accepts `reasoning_effort`.
    pub supports_reasoning_effort: bool,
    /// Streamed responses can include usage via `stream_options`.
    pub supports_usage_in_streaming: bool,
    /// Streamed chunks carry `finish_reason`. When false the stop reason is
    /// inferred from what the turn produced.
    pub supports_finish_reason: bool,
    /// Tool definitions accept `strict`.
    pub supports_strict_mode: bool,
    /// Endpoint honours long prompt-cache retention.
    pub supports_long_cache_retention: bool,
    /// Tool results must repeat the tool `name`.
    pub requires_tool_result_name: bool,
    /// A user message may not directly follow a tool result.
    pub requires_assistant_after_tool_result: bool,
    /// Thinking blocks must be replayed as text wrapped in `<thinking>`.
    pub requires_thinking_as_text: bool,
    /// Replayed assistant messages must carry a (possibly empty)
    /// `reasoning_content` field once reasoning is enabled.
    pub requires_reasoning_content_on_assistant_messages: bool,
    /// Endpoint accepts `temperature` while thinking is enabled. DeepSeek
    /// rejects sampling parameters in thinking mode.
    pub supports_temperature_while_thinking: bool,
    pub max_tokens_field: MaxTokensField,
    pub thinking_format: ThinkingFormat,
}

impl OpenAiCompletionsCompat {
    /// DeepSeek's profile.
    #[must_use]
    pub const fn deepseek() -> Self {
        Self {
            supports_store: false,
            supports_developer_role: false,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            supports_finish_reason: true,
            supports_strict_mode: true,
            supports_long_cache_retention: true,
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant_messages: true,
            supports_temperature_while_thinking: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            thinking_format: ThinkingFormat::DeepSeek,
        }
    }
}

impl Default for OpenAiCompletionsCompat {
    /// OpenAI's own behaviour.
    fn default() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            supports_finish_reason: true,
            supports_strict_mode: true,
            supports_long_cache_retention: true,
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant_messages: false,
            supports_temperature_while_thinking: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            thinking_format: ThinkingFormat::OpenAi,
        }
    }
}

/// A model's compatibility profile, keyed by the API family it belongs to.
///
/// One variant today; the enum is the seam where other protocols attach.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Compat {
    #[default]
    None,
    OpenAiCompletions(Box<OpenAiCompletionsCompat>),
}

impl Compat {
    /// The OpenAI-completions profile, if this is one.
    #[must_use]
    pub fn openai_completions(&self) -> Option<&OpenAiCompletionsCompat> {
        match self {
            Compat::OpenAiCompletions(c) => Some(c),
            Compat::None => None,
        }
    }
}

impl From<OpenAiCompletionsCompat> for Compat {
    fn from(value: OpenAiCompletionsCompat) -> Self {
        Compat::OpenAiCompletions(Box::new(value))
    }
}
