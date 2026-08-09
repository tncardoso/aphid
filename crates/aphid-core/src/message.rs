//! Message roles, stop reasons, and the per-role metadata side tables.

use std::ops::Range;

use compact_str::CompactString;

use crate::error::Diagnostic;
use crate::id::Timestamp;
use crate::json::Json;
use crate::provider::{Api, ProviderId};
use crate::usage::Usage;

/// Who produced a message.
///
/// The system prompt is not special-cased anywhere in aphid: it is simply a
/// message with [`Role::System`], stored in the arena and iterated like any
/// other. Mapping it onto a wire format is the provider encoder's job.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    ToolResult,
}

/// Why a model stopped generating.
#[derive(
    Copy, Clone, PartialEq, Eq, Hash, Debug, Default, serde::Serialize, serde::Deserialize,
)]
pub enum StopReason {
    /// Still streaming.
    #[default]
    Pending,
    /// The model finished its turn.
    Stop,
    /// The output token limit was hit.
    Length,
    /// The model wants one or more tools invoked.
    ToolUse,
    /// The request failed; see [`AssistantMeta::error_message`].
    Error,
    /// The caller cancelled the request.
    Aborted,
}

impl StopReason {
    /// Whether this reason terminates the stream unsuccessfully.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, StopReason::Error | StopReason::Aborted)
    }
}

/// The dense per-message record stored in a transcript's message array.
///
/// Deliberately small: role, the block range, when it happened, and an index
/// into whichever metadata side table the role implies. Heavy per-role
/// metadata never touches user or system messages.
#[derive(Clone, Debug)]
pub(crate) struct MessageHeader {
    pub(crate) role: Role,
    pub(crate) timestamp: Timestamp,
    pub(crate) blocks: Range<u32>,
    /// Index into `assistant_meta` or `tool_result_meta` per `role`;
    /// [`MessageHeader::NO_META`] for system and user messages.
    pub(crate) meta: u32,
}

impl MessageHeader {
    pub(crate) const NO_META: u32 = u32::MAX;
}

/// Everything an assistant turn records beyond its content.
#[derive(Clone, Debug)]
pub struct AssistantMeta {
    pub api: Api,
    pub provider: ProviderId,
    pub model: CompactString,
    /// Concrete model when it differs from the one requested, as with routing
    /// gateways resolving an alias.
    pub response_model: Option<CompactString>,
    pub response_id: Option<CompactString>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// The provider's own stop string, kept for debugging when it does not map
    /// cleanly onto [`StopReason`].
    pub raw_stop_reason: Option<CompactString>,
    /// Provider signal that the model explicitly ended its turn.
    pub end_turn: Option<bool>,
    pub error_message: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

impl AssistantMeta {
    /// A pending turn against `model`, to be filled in as the stream proceeds.
    #[must_use]
    pub fn new(api: Api, provider: ProviderId, model: impl Into<CompactString>) -> Self {
        Self {
            api,
            provider,
            model: model.into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            raw_stop_reason: None,
            end_turn: None,
            error_message: None,
            diagnostics: Vec::new(),
        }
    }
}

/// Everything a tool-result message records beyond its content.
#[derive(Clone, Debug)]
pub struct ToolResultMeta {
    pub tool_call_id: CompactString,
    pub tool_name: CompactString,
    pub is_error: bool,
    /// Cost of running the tool itself, where it is known. Not part of model
    /// context accounting.
    pub usage: Option<Usage>,
    /// Structured payload for consumers that understand this tool.
    pub details: Option<Json>,
    /// Tools that became callable as a result of this one running.
    pub added_tool_names: Vec<CompactString>,
}

impl ToolResultMeta {
    /// A successful result for the given call.
    #[must_use]
    pub fn new(
        tool_call_id: impl Into<CompactString>,
        tool_name: impl Into<CompactString>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            is_error: false,
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
        }
    }

    /// Mark this result as a failure.
    #[must_use]
    pub fn as_error(mut self) -> Self {
        self.is_error = true;
        self
    }
}
