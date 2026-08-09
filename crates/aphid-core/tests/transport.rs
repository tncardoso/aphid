//! End-to-end transport: a real HTTP request, a real SSE body, real events.
//!
//! The server is a throwaway TCP listener rather than a mock of our own types,
//! so this covers the parts unit tests cannot: header handling, body framing,
//! and chunk boundaries falling in the middle of an SSE event.

use std::pin::Pin;
use std::task::Context;
use std::time::Duration;

use aphid_core::providers::deepseek;
use aphid_core::{
    AssistantStream, BlockKind, ContentRef, Event, Model, SimpleStreamOptions, StopReason,
    Transcript, api,
};
use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one request, writing `chunks` back with a pause between each so the
/// client is forced to reassemble across TCP reads. Returns the request body.
async fn serve_once(chunks: Vec<String>) -> (Model, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];

        // Read headers, then exactly as many body bytes as declared.
        let (head_end, content_length) = loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                break (end + 4, length);
            }
            assert!(n > 0, "client closed before sending headers");
        };
        while raw.len() < head_end + content_length {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "client closed mid-body");
            raw.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8_lossy(&raw).into_owned();

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for chunk in chunks {
            socket.write_all(chunk.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        socket.shutdown().await.unwrap();
        request
    });

    let mut model = deepseek::flash();
    model.base_url = format!("http://127.0.0.1:{port}");
    (model, handle)
}

async fn collect(stream: &mut api::CompletionStream) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(event) =
        std::future::poll_fn(|cx: &mut Context<'_>| Pin::new(&mut *stream).poll_next(cx)).await
    {
        events.push(event);
    }
    events
}

fn prompt() -> Transcript {
    let mut t = Transcript::new();
    t.push_user("what is 2 + 2?");
    t
}

#[tokio::test]
async fn a_full_turn_streams_through_http_and_commits() {
    // Deliberately awkward framing: the first event is split mid-JSON.
    let (model, server) = serve_once(vec![
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"delta\":{\"reasoning_c".into(),
        "ontent\":\"simple sum\"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"It is \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"4.\"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".into(),
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18,\"prompt_cache_hit_tokens\":8}}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ])
    .await;

    let mut options = SimpleStreamOptions::default();
    options.stream.request.api_key = Some("test-key".into());
    let mut transcript = prompt();

    let mut stream = api::stream(&model, &transcript, &[], &options).await;
    let events = collect(&mut stream).await;

    assert_eq!(events.first(), Some(&Event::Start));
    assert_eq!(
        events.last(),
        Some(&Event::Done {
            stop: StopReason::Stop
        })
    );
    assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);

    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::BlockStart { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [BlockKind::Thinking, BlockKind::Text]);

    let id = transcript.commit(stream.finish());
    let message = transcript.message(id);
    let blocks: Vec<_> = message.content().filter_map(|c| c.text()).collect();
    assert_eq!(blocks, ["simple sum", "It is 4."]);

    let meta = message.assistant().unwrap();
    assert_eq!(meta.stop_reason, StopReason::Stop);
    assert_eq!(meta.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(meta.usage.cache_read, 8);
    assert_eq!(meta.usage.input, 3);
    assert_eq!(meta.usage.output, 7);
    assert!(meta.error_message.is_none());

    // The server saw a well-formed, authenticated request.
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
    assert!(
        request
            .to_lowercase()
            .contains("authorization: bearer test-key")
    );
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed["messages"][0]["content"], "what is 2 + 2?");
    assert_eq!(parsed["stream"], true);
}

#[tokio::test]
async fn a_tool_call_arrives_as_one_block() {
    let (model, server) = serve_once(vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Lisbon\\\"}\"}}]}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n".into(),
    ])
    .await;

    let mut transcript = prompt();
    let mut stream = api::stream(&model, &transcript, &[], &SimpleStreamOptions::default()).await;
    let events = collect(&mut stream).await;
    assert_eq!(
        events.last(),
        Some(&Event::Done {
            stop: StopReason::ToolUse
        })
    );

    let id = transcript.commit(stream.finish());
    let message = transcript.message(id);
    let ContentRef::ToolCall(call) = message.content().next().unwrap() else {
        panic!("expected a tool call");
    };
    assert_eq!(call.name(), "get_weather");
    assert_eq!(call.arguments().unwrap()["city"], "Lisbon");
    server.await.unwrap();
}

#[tokio::test]
async fn an_http_error_becomes_an_error_event_not_a_panic() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 32\r\nconnection: close\r\n\r\n\
                  {\"error\":\"invalid api key\"}      ",
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });

    let mut model = deepseek::flash();
    model.base_url = format!("http://127.0.0.1:{port}");

    let mut stream = api::stream(&model, &prompt(), &[], &SimpleStreamOptions::default()).await;
    let events = collect(&mut stream).await;

    assert_eq!(
        events,
        [
            Event::Start,
            Event::Error {
                stop: StopReason::Error
            }
        ]
    );
    let turn = stream.finish();
    let error = turn.meta().error_message.as_deref().unwrap();
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("invalid api key"), "{error}");
    server.await.unwrap();
}

#[tokio::test]
async fn a_truncated_stream_is_reported_rather_than_passed_off_as_complete() {
    let (model, server) = serve_once(vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}\n\n".into(),
    ])
    .await;

    let mut stream = api::stream(&model, &prompt(), &[], &SimpleStreamOptions::default()).await;
    collect(&mut stream).await;

    let turn = stream.finish();
    assert_eq!(
        turn.meta().error_message.as_deref(),
        Some("stream ended without a finish reason")
    );
    // What did arrive is still readable.
    assert_eq!(
        turn.partial().content().next().unwrap().text(),
        Some("half an ans")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_through_the_stream() {
    let mut model = deepseek::flash();
    // Port 1 on loopback: nothing is listening.
    model.base_url = "http://127.0.0.1:1".to_owned();

    let mut stream = api::stream(&model, &prompt(), &[], &SimpleStreamOptions::default()).await;
    let events = collect(&mut stream).await;
    assert_eq!(
        events,
        [
            Event::Start,
            Event::Error {
                stop: StopReason::Error
            }
        ]
    );
    assert!(
        stream
            .finish()
            .meta()
            .error_message
            .as_deref()
            .unwrap()
            .contains("request failed")
    );
}
