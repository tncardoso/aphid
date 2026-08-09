//! The JSON value type used for tool schemas, tool details and provider metadata.
//!
//! Aliased rather than re-exported directly so the backing implementation can be
//! swapped for an in-house zero-copy value type without touching call sites.
//!
//! Note what is *not* here: streamed tool-call arguments are kept as raw text in
//! the transcript arena and parsed only on demand, via
//! [`ToolCallRef::arguments`](crate::ToolCallRef::arguments).

/// A parsed JSON value.
pub type Json = serde_json::Value;

/// Failure to parse or serialize JSON.
pub type JsonError = serde_json::Error;

/// Parse a JSON document.
///
/// # Errors
/// Returns the underlying parse error when `text` is not valid JSON.
pub fn parse(text: &str) -> Result<Json, JsonError> {
    serde_json::from_str(text)
}
