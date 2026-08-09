//! HTTP transport for the OpenAI Chat Completions protocol.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;

use crate::api::openai_completions::{CHAT_COMPLETIONS_PATH, ChunkDecoder, encode_request};
use crate::api::sse::SseDecoder;
use crate::buffer::MessageBuffer;
use crate::event::{AssistantStream, Event};
use crate::message::{AssistantMeta, StopReason};
use crate::model::Model;
use crate::options::SimpleStreamOptions;
use crate::span::Span;
use crate::tool::Tool;
use crate::transcript::Transcript;
use crate::view::MessageRef;

/// One connection pool for the process. Building a client is expensive enough
/// that a per-request one would show up in the startup budget.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("aphid/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("default reqwest client builds")
    })
}

/// A streamed completion.
///
/// Upholds the [`AssistantStream`] contract: [`Event::Start`] first, exactly one
/// [`Event::Done`] or [`Event::Error`] last, and every failure — encoding,
/// transport, HTTP status, malformed chunk — reported through the stream rather
/// than thrown.
pub struct CompletionStream {
    body: Option<Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>>,
    sse: SseDecoder,
    decoder: ChunkDecoder,
    buffer: MessageBuffer,
    model: Model,
    pending: VecDeque<Event>,
    started: bool,
    finished: bool,
}

/// Send a request and return its event stream.
///
/// Never fails: problems are encoded into the returned stream, so callers have
/// exactly one path to handle.
pub async fn stream(
    model: &Model,
    transcript: &Transcript,
    tools: &[Tool],
    options: &SimpleStreamOptions,
) -> CompletionStream {
    let meta = AssistantMeta::new(model.api.clone(), model.provider.clone(), model.id.clone());
    let buffer = MessageBuffer::new(meta);

    let body = match encode_request(model, transcript, tools, options) {
        Ok(body) => body,
        Err(error) => return CompletionStream::failed(model, buffer, format!("{error}")),
    };

    let url = format!(
        "{}{CHAT_COMPLETIONS_PATH}",
        model.base_url.trim_end_matches('/')
    );
    let mut request = client()
        .post(&url)
        .header("content-type", "application/json");

    if let Some(key) = &options.stream.request.api_key {
        request = request.bearer_auth(key);
    }
    for (name, value) in &model.headers {
        request = request.header(name.as_str(), value);
    }
    for (name, value) in &options.stream.request.headers {
        request = match value {
            Some(value) => request.header(name.as_str(), value),
            // A `None` override suppresses a default header of the same name.
            None => request,
        };
    }
    if let Some(timeout) = options.stream.request.timeout {
        request = request.timeout(timeout);
    }

    let response = match request.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            return CompletionStream::failed(model, buffer, format!("request failed: {error}"));
        }
    };

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail = detail.trim();
        let message = if detail.is_empty() {
            format!("provider returned HTTP {status}")
        } else {
            format!("provider returned HTTP {status}: {detail}")
        };
        return CompletionStream::failed(model, buffer, message);
    }

    CompletionStream {
        body: Some(Box::pin(response.bytes_stream())),
        sse: SseDecoder::default(),
        decoder: ChunkDecoder::default(),
        buffer,
        model: model.clone(),
        pending: VecDeque::new(),
        started: false,
        finished: false,
    }
}

impl CompletionStream {
    /// The request body that was (or would have been) sent. Useful for a debug
    /// flag; it costs a full re-encode, so it is not on any hot path.
    ///
    /// # Errors
    /// Propagates whatever [`encode_request`] rejects.
    pub fn preview_request(
        model: &Model,
        transcript: &Transcript,
        tools: &[Tool],
        options: &SimpleStreamOptions,
    ) -> crate::Result<String> {
        encode_request(model, transcript, tools, options)
    }

    fn failed(model: &Model, mut buffer: MessageBuffer, message: String) -> Self {
        buffer.meta_mut().stop_reason = StopReason::Error;
        buffer.meta_mut().error_message = Some(message);
        let mut pending = VecDeque::new();
        pending.push_back(Event::Error {
            stop: StopReason::Error,
        });
        Self {
            body: None,
            sse: SseDecoder::default(),
            decoder: ChunkDecoder::default(),
            buffer,
            model: model.clone(),
            pending,
            started: false,
            finished: true,
        }
    }

    /// Consume whatever complete SSE events have arrived.
    fn drain_sse(&mut self) {
        while let Some(payload) = self.sse.next_event() {
            if let Err(error) =
                self.decoder
                    .apply(&payload, &mut self.buffer, &self.model, &mut self.pending)
            {
                self.fail(format!("malformed chunk: {error}"));
                return;
            }
        }
    }

    /// The body ended cleanly: close blocks and emit the terminal event.
    fn finish_stream(&mut self) {
        if let Some(payload) = self.sse.flush() {
            let _ = self
                .decoder
                .apply(&payload, &mut self.buffer, &self.model, &mut self.pending);
        }
        self.decoder.close_open_blocks(&mut self.pending);

        let stop = self.decoder.stop_reason();
        self.buffer.meta_mut().stop_reason = stop;
        if !self.decoder.saw_done() && self.buffer.meta().raw_stop_reason.is_none() {
            // The connection ended without a finish reason or a sentinel; the
            // reply may be truncated, so say so rather than claim a clean stop.
            self.buffer.meta_mut().error_message =
                Some("stream ended without a finish reason".to_owned());
        }
        self.pending.push_back(Event::Done { stop });
        self.finished = true;
        self.body = None;
    }

    fn fail(&mut self, message: String) {
        self.decoder.close_open_blocks(&mut self.pending);
        self.buffer.meta_mut().stop_reason = StopReason::Error;
        self.buffer.meta_mut().error_message = Some(message);
        self.pending.push_back(Event::Error {
            stop: StopReason::Error,
        });
        self.finished = true;
        self.body = None;
    }
}

impl Stream for CompletionStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let this = self.get_mut();

        if !this.started {
            this.started = true;
            return Poll::Ready(Some(Event::Start));
        }

        loop {
            if let Some(event) = this.pending.pop_front() {
                return Poll::Ready(Some(event));
            }
            if this.finished {
                return Poll::Ready(None);
            }

            let polled = match this.body.as_mut() {
                Some(body) => body.as_mut().poll_next(cx),
                None => {
                    this.finish_stream();
                    continue;
                }
            };

            match polled {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.sse.push(&bytes);
                    this.drain_sse();
                }
                Poll::Ready(Some(Err(error))) => this.fail(format!("stream error: {error}")),
                Poll::Ready(None) => this.finish_stream(),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AssistantStream for CompletionStream {
    fn text(&self, span: Span) -> &str {
        self.buffer.text(span)
    }

    fn partial(&self) -> MessageRef<'_> {
        self.buffer.partial()
    }

    fn finish(self) -> MessageBuffer {
        self.buffer
    }
}
