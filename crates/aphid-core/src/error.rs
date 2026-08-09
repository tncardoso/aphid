//! Crate errors and the structured diagnostics attached to assistant messages.

use compact_str::CompactString;

use crate::id::Timestamp;
use crate::json::{Json, JsonError};

/// Errors surfaced by the core type layer.
///
/// Arena overflow is deliberately *not* here: exceeding 4 GiB in one
/// conversation is a programming error, so the arenas panic with a clear
/// message rather than making every append fallible.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A message index that does not exist in this transcript.
    #[error("no message at index {0}")]
    UnknownMessage(u32),

    /// Tool-call arguments, or a provider chunk, were not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] JsonError),

    /// A message holds content this wire protocol cannot represent.
    #[error("{0} content is not supported by this API")]
    UnsupportedContent(&'static str),
}

/// Convenience alias for fallible core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// The error part of a [`Diagnostic`], flattened for logging and replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticError {
    pub name: Option<CompactString>,
    pub message: String,
    pub code: Option<CompactString>,
}

/// A redacted, structured record of something that went wrong or was recovered
/// from during a turn.
///
/// Diagnostics hang off [`AssistantMeta`](crate::AssistantMeta) so a degraded
/// response stays inspectable after the fact — the "full debuggability" goal.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Short machine-readable kind, e.g. `"retry"` or `"stream_reset"`.
    pub kind: CompactString,
    pub timestamp: Timestamp,
    pub error: Option<DiagnosticError>,
    pub details: Option<Json>,
}

impl Diagnostic {
    /// Record a diagnostic of `kind` as of now.
    #[must_use]
    pub fn now(kind: impl Into<CompactString>) -> Self {
        Self {
            kind: kind.into(),
            timestamp: chrono::Utc::now(),
            error: None,
            details: None,
        }
    }

    #[must_use]
    pub fn with_error(mut self, error: DiagnosticError) -> Self {
        self.error = Some(error);
        self
    }

    #[must_use]
    pub fn with_details(mut self, details: Json) -> Self {
        self.details = Some(details);
        self
    }
}
