//! The streaming protocol: span-carrying events, the lent partial view, and the
//! single-memcpy commit.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use aphid_core::{
    Api, AssistantMeta, AssistantStream, BlockKind, ContentRef, Event, MessageBuffer, ProviderId,
    Span, StopReason, Transcript,
};
use futures_core::Stream;

fn meta() -> AssistantMeta {
    AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "deepseek-v4-flash",
    )
}

/// What a provider adapter does, minus the network: drive a [`MessageBuffer`]
/// and emit events naming the bytes it just wrote.
///
/// Its real job in this test suite is to prove [`AssistantStream`] is
/// implementable — that lending `partial()` out of a `Stream` actually borrow-checks.
struct ScriptedStream {
    steps: VecDeque<Step>,
    buffer: MessageBuffer,
    started: bool,
}

enum Step {
    OpenText,
    OpenToolCall(&'static str, &'static str),
    Delta(u32, &'static str),
    Close(u32),
    Finish(StopReason),
}

impl ScriptedStream {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            buffer: MessageBuffer::new(meta()),
            started: false,
        }
    }
}

impl Stream for ScriptedStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let this = self.get_mut();
        if !this.started {
            this.started = true;
            return Poll::Ready(Some(Event::Start));
        }
        let Some(step) = this.steps.pop_front() else {
            return Poll::Ready(None);
        };
        let event = match step {
            Step::OpenText => {
                let index = this.buffer.begin_text();
                Event::BlockStart {
                    index,
                    kind: BlockKind::Text,
                }
            }
            Step::OpenToolCall(id, name) => {
                let index = this.buffer.begin_tool_call(id, name);
                Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall,
                }
            }
            Step::Delta(index, text) => {
                let span = this.buffer.push_delta(index, text);
                Event::Delta {
                    index,
                    kind: this.buffer.block_kind(index),
                    span,
                }
            }
            Step::Close(index) => Event::BlockEnd { index },
            Step::Finish(stop) => {
                this.buffer.meta_mut().stop_reason = stop;
                if stop.is_failure() {
                    Event::Error { stop }
                } else {
                    Event::Done { stop }
                }
            }
        };
        Poll::Ready(Some(event))
    }
}

impl AssistantStream for ScriptedStream {
    fn text(&self, span: Span) -> &str {
        self.buffer.text(span)
    }

    fn partial(&self) -> aphid_core::MessageRef<'_> {
        self.buffer.partial()
    }

    fn finish(self) -> MessageBuffer {
        self.buffer
    }
}

/// Drain a stream synchronously; nothing here actually awaits.
fn drain(stream: &mut ScriptedStream, mut on_event: impl FnMut(Event, &ScriptedStream)) {
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match Pin::new(&mut *stream).poll_next(&mut cx) {
            Poll::Ready(Some(event)) => on_event(event, stream),
            Poll::Ready(None) => break,
            Poll::Pending => unreachable!("scripted stream never pends"),
        }
    }
}

#[test]
fn delta_spans_resolve_to_exactly_the_bytes_just_written() {
    let mut stream = ScriptedStream::new([
        Step::OpenText,
        Step::Delta(0, "Hel"),
        Step::Delta(0, "lo, "),
        Step::Delta(0, "world"),
        Step::Close(0),
        Step::Finish(StopReason::Stop),
    ]);

    let mut deltas = Vec::new();
    drain(&mut stream, |event, s| {
        if let Event::Delta { span, kind, .. } = event {
            assert_eq!(kind, BlockKind::Text);
            deltas.push(s.text(span).to_owned());
        }
    });

    assert_eq!(deltas, ["Hel", "lo, ", "world"]);
}

#[test]
fn partial_tracks_the_incremental_reconstruction() {
    let mut stream = ScriptedStream::new([
        Step::OpenText,
        Step::Delta(0, "one "),
        Step::Delta(0, "two"),
        Step::Close(0),
        Step::Finish(StopReason::Stop),
    ]);

    let mut accumulated = String::new();
    let mut snapshots = Vec::new();
    drain(&mut stream, |event, s| {
        if let Event::Delta { span, .. } = event {
            accumulated.push_str(s.text(span));
        }
        // The lent snapshot must always agree with replaying the deltas.
        let partial: String = s.partial().content().filter_map(|c| c.text()).collect();
        assert_eq!(partial, accumulated);
        snapshots.push(partial);
    });

    assert_eq!(snapshots.last().unwrap(), "one two");
}

#[test]
fn a_committed_turn_is_identical_to_the_staged_one() {
    let mut stream = ScriptedStream::new([
        Step::OpenText,
        Step::Delta(0, "thinking out loud"),
        Step::Close(0),
        Step::OpenToolCall("call_7", "grep"),
        Step::Delta(1, r#"{"pattern":"#),
        Step::Delta(1, r#""fn main"}"#),
        Step::Close(1),
        Step::Finish(StopReason::ToolUse),
    ]);
    drain(&mut stream, |_, _| {});

    let staged: Vec<String> = stream
        .partial()
        .content()
        .map(|c| match c {
            ContentRef::Text(t) => t.text().to_owned(),
            ContentRef::ToolCall(tc) => tc.arguments_raw().to_owned(),
            other => panic!("unexpected {:?}", other.kind()),
        })
        .collect();

    let mut transcript = Transcript::new();
    transcript.push_user("go");
    let id = transcript.commit(stream.finish());

    let committed: Vec<String> = transcript
        .message(id)
        .content()
        .map(|c| match c {
            ContentRef::Text(t) => t.text().to_owned(),
            ContentRef::ToolCall(tc) => tc.arguments_raw().to_owned(),
            other => panic!("unexpected {:?}", other.kind()),
        })
        .collect();

    assert_eq!(staged, committed);
    assert_eq!(committed, ["thinking out loud", r#"{"pattern":"fn main"}"#]);
    // Spans were rebased onto a transcript that already held a user message.
    assert_eq!(
        transcript.message(id).assistant().unwrap().stop_reason,
        StopReason::ToolUse
    );
    assert_eq!(transcript.arena_stats().text_garbage_bytes(), 0);
}

#[test]
fn interleaved_blocks_relocate_instead_of_corrupting() {
    // Some providers stream two blocks at once. The buffer must cope, at the
    // cost of copying the stranded prefix to the tail.
    let mut stream = ScriptedStream::new([
        Step::OpenText,
        Step::OpenToolCall("call_1", "ls"),
        Step::Delta(0, "text-a "),
        Step::Delta(1, r#"{"path":"#),
        Step::Delta(0, "text-b"),
        Step::Delta(1, r#""/tmp"}"#),
        Step::Finish(StopReason::ToolUse),
    ]);
    drain(&mut stream, |_, _| {});

    let buffer = stream.finish();
    let partial = buffer.partial();
    let mut blocks = partial.content();
    assert_eq!(blocks.next().unwrap().text(), Some("text-a text-b"));
    let ContentRef::ToolCall(call) = blocks.next().unwrap() else {
        panic!("expected a tool call")
    };
    assert_eq!(call.arguments_raw(), r#"{"path":"/tmp"}"#);
}

#[test]
fn a_failed_turn_terminates_with_error_not_a_panic() {
    let mut stream = ScriptedStream::new([
        Step::OpenText,
        Step::Delta(0, "partial answ"),
        Step::Finish(StopReason::Aborted),
    ]);

    let mut terminal = None;
    drain(&mut stream, |event, _| {
        if event.is_terminal() {
            terminal = Some(event);
        }
    });

    assert_eq!(
        terminal,
        Some(Event::Error {
            stop: StopReason::Aborted
        })
    );
    let buffer = stream.finish();
    assert_eq!(buffer.meta().stop_reason, StopReason::Aborted);
    // Whatever streamed before the failure is still readable.
    assert_eq!(
        buffer.partial().content().next().unwrap().text(),
        Some("partial answ")
    );
}

#[test]
fn events_stay_small_and_sendable() {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<Event>();
    assert!(size_of::<Event>() <= 16);
}
