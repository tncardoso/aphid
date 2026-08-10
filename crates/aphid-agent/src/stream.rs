//! The seam between the agent loop and a provider.
//!
//! The loop never calls [`aphid_core::api::stream`] directly. It goes through a
//! [`Backend`], which defaults to exactly that but can be replaced with a
//! scripted stream in tests, a recording proxy for debugging, or a router.

use std::sync::Arc;

use aphid_core::api::CompletionStream;
use aphid_core::{
    AssistantStream, MessageBuffer, Model, SimpleStreamOptions, Tool, Transcript, api,
};

use crate::tool::BoxFuture;

/// [`AssistantStream`] with an object-safe `finish`.
///
/// [`AssistantStream::finish`] takes `self` by value, so it is excluded from the
/// vtable and cannot be called through a trait object. This blanket extension
/// restores it for `Box<dyn _>` without asking anything of the core.
pub trait DynAssistantStream: AssistantStream {
    /// Take the finished message, ready for
    /// [`Transcript::commit`](aphid_core::Transcript::commit).
    fn finish_boxed(self: Box<Self>) -> MessageBuffer;
}

impl<S: AssistantStream> DynAssistantStream for S {
    fn finish_boxed(self: Box<Self>) -> MessageBuffer {
        (*self).finish()
    }
}

/// A provider response in progress, type-erased.
pub type BoxStream = Box<dyn DynAssistantStream + Send + Unpin>;

/// Sends one request and returns its event stream.
///
/// A trait rather than a boxed closure so an implementation can carry state —
/// a recorded script, a request log, a retry budget — without fighting
/// higher-ranked lifetime inference.
pub trait Backend: Send + Sync + 'static {
    /// Never fails: problems are encoded into the returned stream as
    /// [`Event::Error`](aphid_core::Event::Error), so the loop has exactly one
    /// path to handle.
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        transcript: &'a Transcript,
        tools: &'a [Tool],
        options: &'a SimpleStreamOptions,
    ) -> BoxFuture<'a, BoxStream>;
}

/// A shared backend, as the agent stores it.
pub type StreamFn = Arc<dyn Backend>;

/// The default backend: real HTTP against the model's provider.
#[derive(Copy, Clone, Debug, Default)]
pub struct Live;

impl Backend for Live {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        transcript: &'a Transcript,
        tools: &'a [Tool],
        options: &'a SimpleStreamOptions,
    ) -> BoxFuture<'a, BoxStream> {
        Box::pin(async move {
            let stream: CompletionStream = api::stream(model, transcript, tools, options).await;
            Box::new(stream) as BoxStream
        })
    }
}

/// The default backend, ready to store.
#[must_use]
pub fn live_stream_fn() -> StreamFn {
    Arc::new(Live)
}
