//! Provider wire protocols.
//!
//! Requests are encoded by hand straight out of the transcript arena; responses
//! are decoded with serde into borrowed structs. No vendor SDK is involved.

mod json_writer;
pub mod openai_completions;
mod sse;
mod transport;

pub use openai_completions::encode_request;
pub use transport::{CompletionStream, stream};
