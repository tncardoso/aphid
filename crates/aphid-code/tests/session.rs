//! Session files: what is written, and what comes back.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use aphid_code::session::{self, SessionStore};
use aphid_core::{
    Api, AssistantMeta, ContentInput, ContentRef, MessageBuffer, ProviderId, Role, StopReason,
    ToolResultMeta, Transcript, Usage,
};

struct Temp {
    root: PathBuf,
}

impl Temp {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "aphid-session-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("temp dir");
        Self {
            root: root.canonicalize().expect("canonical"),
        }
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A transcript exercising every content kind and both metadata tables.
fn rich_transcript() -> Transcript {
    let mut transcript = Transcript::new();
    transcript.push_system("You are terse.");
    transcript.push_user_parts(&[
        ContentInput::Text("look at this"),
        ContentInput::Image {
            data: &[0u8, 1, 2, 253, 254, 255],
            mime: "image/png",
        },
    ]);

    let mut meta = AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "deepseek-v4-pro",
    );
    meta.usage = Usage {
        input: 120,
        output: 34,
        cache_read: 8,
        total_tokens: 162,
        ..Usage::default()
    };
    meta.stop_reason = StopReason::ToolUse;
    meta.response_id = Some("resp_1".into());
    meta.end_turn = Some(false);

    let mut buffer = MessageBuffer::new(meta);
    let thinking = buffer.begin_thinking();
    buffer.push_delta(thinking, "weighing it up");
    buffer.set_signature(thinking, "sig-abc");
    let text = buffer.begin_text();
    buffer.push_delta(text, "Let me check.");
    let call = buffer.begin_tool_call("call_1", "read");
    buffer.push_delta(call, r#"{"path":"a.rs"}"#);
    transcript.commit(buffer);

    let mut result = ToolResultMeta::new("call_1", "read");
    result.details = Some(serde_json::json!({ "total_lines": 3 }));
    transcript.push_tool_result(result, &[ContentInput::Text("1\tfn a() {}")]);

    transcript
}

/// Compare everything the format claims to preserve.
fn assert_same(left: &Transcript, right: &Transcript) {
    assert_eq!(left.len(), right.len(), "message count");

    for index in 0..left.len() {
        let a = left.get(index).expect("left message");
        let b = right.get(index).expect("right message");
        assert_eq!(a.role(), b.role(), "role at {index}");
        assert_eq!(a.len(), b.len(), "block count at {index}");

        for (x, y) in a.content().zip(b.content()) {
            match (x, y) {
                (ContentRef::Text(x), ContentRef::Text(y)) => {
                    assert_eq!(x.text(), y.text());
                    assert_eq!(x.signature(), y.signature());
                }
                (ContentRef::Thinking(x), ContentRef::Thinking(y)) => {
                    assert_eq!(x.text(), y.text());
                    assert_eq!(x.signature(), y.signature());
                    assert_eq!(x.redacted(), y.redacted());
                }
                (ContentRef::ToolCall(x), ContentRef::ToolCall(y)) => {
                    assert_eq!(x.id(), y.id());
                    assert_eq!(x.name(), y.name());
                    assert_eq!(x.arguments_raw(), y.arguments_raw());
                }
                (ContentRef::Image(x), ContentRef::Image(y)) => {
                    assert_eq!(x.mime(), y.mime());
                    assert_eq!(x.data(), y.data());
                }
                (x, y) => panic!("block kind changed at {index}: {x:?} vs {y:?}"),
            }
        }

        match (a.assistant(), b.assistant()) {
            (Some(x), Some(y)) => {
                assert_eq!(x.model, y.model);
                assert_eq!(x.provider, y.provider);
                assert_eq!(x.api, y.api);
                assert_eq!(x.usage, y.usage);
                assert_eq!(x.stop_reason, y.stop_reason);
                assert_eq!(x.response_id, y.response_id);
                assert_eq!(x.end_turn, y.end_turn);
                // Assistant turns replay through a MessageBuffer, which carries
                // the original timestamp across.
                assert_eq!(a.timestamp(), b.timestamp());
            }
            (None, None) => {}
            _ => panic!("assistant metadata changed at {index}"),
        }

        match (a.tool_result(), b.tool_result()) {
            (Some(x), Some(y)) => {
                assert_eq!(x.tool_call_id, y.tool_call_id);
                assert_eq!(x.tool_name, y.tool_name);
                assert_eq!(x.is_error, y.is_error);
                assert_eq!(x.details, y.details);
            }
            (None, None) => {}
            _ => panic!("tool result metadata changed at {index}"),
        }
    }
}

#[test]
fn a_session_round_trips_every_content_kind() {
    let temp = Temp::new();
    let original = rich_transcript();

    let mut store =
        SessionStore::create(&temp.root, &temp.root, &temp.root, Some("deepseek-v4-pro"))
            .expect("create");
    store.flush(&original).expect("flush");
    let path = store.path().to_path_buf();

    let mut reloaded = Transcript::new();
    let (_store, header) = SessionStore::resume(&path, &mut reloaded).expect("resume");

    assert_eq!(header.cwd, temp.root.display().to_string());
    assert_eq!(header.model.as_deref(), Some("deepseek-v4-pro"));
    assert_same(&original, &reloaded);
}

#[test]
fn flushing_only_appends_what_is_new() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_user("one");

    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("first flush");
    let after_one = std::fs::read_to_string(store.path()).expect("read");

    transcript.push_user("two");
    store.flush(&transcript).expect("second flush");
    let after_two = std::fs::read_to_string(store.path()).expect("read");

    assert!(
        after_two.starts_with(&after_one),
        "the first write was not rewritten"
    );
    assert_eq!(after_two.lines().count(), 3, "header plus two messages");

    // Flushing again with nothing new adds nothing.
    store.flush(&transcript).expect("third flush");
    assert_eq!(
        std::fs::read_to_string(store.path()).expect("read"),
        after_two
    );
}

#[test]
fn resuming_continues_appending_to_the_same_file() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_user("one");

    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");
    let path = store.path().to_path_buf();
    drop(store);

    let mut reloaded = Transcript::new();
    let (mut store, _) = SessionStore::resume(&path, &mut reloaded).expect("resume");
    reloaded.push_user("two");
    store.flush(&reloaded).expect("flush");

    let mut again = Transcript::new();
    SessionStore::resume(&path, &mut again).expect("resume again");
    assert_eq!(again.len(), 2);
    assert_eq!(again.get(1).unwrap().role(), Role::User);
}

#[test]
fn a_truncated_line_does_not_stop_the_load() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_user("one");

    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");
    let path = store.path().to_path_buf();

    // Simulate a crash mid-write.
    let mut text = std::fs::read_to_string(&path).expect("read");
    text.push_str("{\"kind\":\"message\",\"role\":\"Us");
    std::fs::write(&path, text).expect("write");

    let mut reloaded = Transcript::new();
    SessionStore::resume(&path, &mut reloaded).expect("resume");
    assert_eq!(reloaded.len(), 1, "the intact message survived");
}

#[test]
fn sessions_are_listed_newest_first_and_found_by_cwd_or_id() {
    let temp = Temp::new();
    let elsewhere = temp.root.join("other");

    let mut first = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    let mut transcript = Transcript::new();
    transcript.push_user("hello");
    first.flush(&transcript).expect("flush");

    let second = SessionStore::create(&temp.root, &temp.root, &elsewhere, None).expect("create");

    let all = session::list(&temp.root);
    assert_eq!(all.len(), 2);

    let for_cwd = session::newest_for(&temp.root, &temp.root).expect("found by cwd");
    assert_eq!(for_cwd.header.id, first.id());
    assert_eq!(for_cwd.messages, 1);

    let by_id = session::resolve(&temp.root, second.id()).expect("found by id");
    assert_eq!(by_id.header.cwd, elsewhere.display().to_string());

    // A prefix is enough.
    let prefix = &second.id()[..8];
    assert!(session::resolve(&temp.root, prefix).is_some());
    assert!(session::resolve(&temp.root, "nope").is_none());
}

#[test]
fn listing_for_a_project_is_not_fooled_by_a_shared_name_prefix() {
    let temp = Temp::new();
    // The whole point: "app" is a prefix of "app-backend"'s directory name, so
    // a filter on the filename's prefix would wrongly let app-backend's
    // sessions leak into app's listing.
    let app = temp.root.join("app");
    let backend = temp.root.join("app-backend");

    let app_store = SessionStore::create(&temp.root, &app, &app, None).expect("create");
    let backend_store = SessionStore::create(&temp.root, &backend, &backend, None).expect("create");

    let for_app = session::list_for(&temp.root, &app);
    assert_eq!(for_app.len(), 1, "{for_app:?}");
    assert_eq!(for_app[0].header.id, app_store.id());

    let for_backend = session::list_for(&temp.root, &backend);
    assert_eq!(for_backend.len(), 1, "{for_backend:?}");
    assert_eq!(for_backend[0].header.id, backend_store.id());
}

#[test]
fn a_transcript_that_shrank_rewinds_instead_of_interleaving() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_user("one");
    transcript.push_user("two");

    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");

    transcript.truncate(1);
    store.flush(&transcript).expect("flush after truncate");
    transcript.push_user("replacement");
    store.flush(&transcript).expect("flush replacement");

    let text = std::fs::read_to_string(store.path()).expect("read");
    assert!(text.contains("replacement"));
    assert_eq!(text.lines().count(), 4, "header, two, then the replacement");
}
