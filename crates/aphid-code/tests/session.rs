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
    let (_store, header) = SessionStore::resume(&path, None, &mut reloaded).expect("resume");

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
    let (mut store, _) = SessionStore::resume(&path, None, &mut reloaded).expect("resume");
    reloaded.push_user("two");
    store.flush(&reloaded).expect("flush");

    let mut again = Transcript::new();
    SessionStore::resume(&path, None, &mut again).expect("resume again");
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
    SessionStore::resume(&path, None, &mut reloaded).expect("resume");
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
fn a_transcript_that_shrank_branches_instead_of_interleaving() {
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

    // The file keeps both branches, and the head is on the new one.
    let (_, reloaded) = session::load(store.path()).expect("load");
    let texts: Vec<_> = reloaded
        .iter()
        .map(|message| first_text(&message))
        .collect();
    assert_eq!(texts, ["one", "replacement"]);

    let tree = session::Tree::read(store.path()).expect("tree");
    assert_eq!(tree.nodes.len(), 3);
    assert_eq!(tree.nodes[0].children.len(), 2, "one has two children");
}

fn first_text(message: &aphid_core::MessageRef<'_>) -> String {
    message
        .content()
        .find_map(|block| match block {
            ContentRef::Text(text) => Some(text.text().to_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

fn texts(transcript: &Transcript) -> Vec<String> {
    transcript
        .iter()
        .map(|message| first_text(&message))
        .collect()
}

#[test]
fn a_file_from_before_trees_reads_as_a_line_and_takes_a_fork() {
    let temp = Temp::new();
    let path = temp.root.join("old.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"kind":"session","id":"20250101T000000-0000","cwd":"/x","started":"2025-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"kind":"message","role":"User","ts":"2025-01-01T00:00:01Z","content":[{"type":"text","text":"first"}]}"#,
            "\n",
            r#"{"kind":"message","role":"User","ts":"2025-01-01T00:00:02Z","content":[{"type":"text","text":"second"}]}"#,
            "\n",
        ),
    )
    .expect("write");

    let contents = session::read(&path).expect("read");
    assert_eq!(contents.records[0].id.as_deref(), Some("0"));
    assert_eq!(contents.records[1].parent.as_deref(), Some("0"));
    assert_eq!(contents.head.as_deref(), Some("1"));

    // Continue from the first message: the new one is its child.
    let mut transcript = Transcript::new();
    let (mut store, _) = SessionStore::resume(&path, Some("0"), &mut transcript).expect("resume");
    assert_eq!(texts(&transcript), ["first"]);
    transcript.push_user("other second");
    store.flush(&transcript).expect("flush");

    let tree = session::Tree::read(&path).expect("tree");
    assert_eq!(tree.nodes[0].children.len(), 2);
    let (_, head) = session::load(&path).expect("load");
    assert_eq!(texts(&head), ["first", "other second"]);
}

#[test]
fn a_head_line_moves_where_a_resume_continues() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_user("a");
    transcript.push_user("b");
    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");
    let first = store.line()[0].clone().expect("an id");

    store.rewind(1);
    store.mark_head().expect("head");

    let (_, reloaded) = session::load(store.path()).expect("load");
    assert_eq!(texts(&reloaded), ["a"]);
    let (_, all) = session::load_at(store.path(), Some(&first)).expect("load at");
    assert_eq!(texts(&all), ["a"]);
}

#[test]
fn two_writers_on_one_file_keep_their_branches_apart() {
    let temp = Temp::new();
    let mut base = Transcript::new();
    base.push_user("root");
    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&base).expect("flush");
    let path = store.path().to_path_buf();
    let root = store.line()[0].clone().expect("an id");

    let mut other = Transcript::new();
    let (mut second, _) = SessionStore::resume(&path, Some(&root), &mut other).expect("resume");

    base.push_user("left");
    other.push_user("right");
    store.flush(&base).expect("flush left");
    second.flush(&other).expect("flush right");
    base.push_user("left again");
    store.flush(&base).expect("flush left again");

    let tree = session::Tree::read(&path).expect("tree");
    assert_eq!(tree.nodes.len(), 4);
    assert_eq!(tree.nodes[0].children.len(), 2);
    let ids: std::collections::HashSet<_> = tree.nodes.iter().map(|node| &node.id).collect();
    assert_eq!(ids.len(), 4, "every id is unique");
    let (_, head) = session::load(&path).expect("load");
    assert_eq!(texts(&head), ["root", "left", "left again"]);
}

#[test]
fn the_skim_reads_previews_and_tool_calls_without_the_content() {
    let temp = Temp::new();
    let transcript = rich_transcript();
    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");

    let tree = session::Tree::read(store.path()).expect("tree");
    assert_eq!(tree.nodes.len(), 4);
    assert_eq!(tree.nodes[1].preview, "look at this");
    assert_eq!(tree.nodes[2].tool_calls, 1);
    assert!(!tree.nodes[2].ends_turn());
    assert!(tree.nodes[0].ends_turn(), "a system prompt ends a turn");
}

/// An assistant answer that ends its turn.
fn answer(text: &str) -> MessageBuffer {
    let mut buffer = MessageBuffer::new(AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "m",
    ));
    let index = buffer.begin_text();
    buffer.push_delta(index, text);
    buffer
}

#[test]
fn the_view_groups_messages_into_turns_and_marks_the_head() {
    let temp = Temp::new();
    let mut transcript = Transcript::new();
    transcript.push_system("sys");
    transcript.push_user("q1");
    transcript.commit(answer("a1"));
    transcript.push_user("q2");
    transcript.commit(answer("a2"));
    let mut store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    store.flush(&transcript).expect("flush");

    // Branch after the first answer.
    transcript.truncate(3);
    store.rewind(3);
    transcript.push_user("q2 again");
    store.flush(&transcript).expect("flush");
    let first_prompt = store.line()[1].clone().expect("id");
    store.label(&first_prompt, "the plan").expect("label");

    let view = session::Tree::read(store.path()).expect("tree").view();
    assert_eq!(view.title, "the plan");
    assert_eq!(view.turns.len(), 3);
    let first = &view.turns[0];
    assert_eq!(first.prompt, "q1");
    assert_eq!(first.reply, "a1");
    assert!(first.end.is_some());
    assert!(first.on_head);
    assert_eq!(view.children(Some(&first.id)).count(), 2);
    let head = view.turns.iter().find(|turn| turn.is_head).expect("a head");
    assert_eq!(head.prompt, "q2 again");
    assert!(head.end.is_none(), "no answer yet");
    assert_eq!(
        view.branch_start().map(|turn| turn.prompt.as_str()),
        Some("q2 again")
    );
    assert!(!view.turns[1].on_head);
}

#[test]
fn an_address_splits_into_session_and_message() {
    assert_eq!(session::split_address("abc:12ef"), ("abc", Some("12ef")));
    assert_eq!(session::split_address("abc"), ("abc", None));
    assert_eq!(session::split_address("abc:"), ("abc", None));
    assert_eq!(
        session::split_address("abc:12ef:99aa"),
        ("abc:12ef", Some("99aa")),
        "a fork's id keeps its own address"
    );
}

/// An agent with a system prompt and a session that writes for it, flushed by
/// hand where a run would flush at each moment.
fn agent_with_session(
    temp: &Temp,
) -> (
    aphid_agent::Agent,
    std::sync::Arc<session::SessionComponent>,
) {
    let agent = aphid_agent::Agent::builder()
        .model(aphid_core::providers::deepseek::flash())
        .system("sys")
        .build();
    let store = SessionStore::create(&temp.root, &temp.root, &temp.root, None).expect("create");
    let listeners = std::sync::Arc::new(aphid_agent::TranscriptListeners::default());
    (
        agent,
        std::sync::Arc::new(session::SessionComponent::new(store, listeners)),
    )
}

fn flush(agent: &aphid_agent::Agent, session: &session::SessionComponent) {
    session
        .with_store(|store| store.flush(agent.transcript()))
        .expect("lock")
        .expect("flush");
}

fn node(session: &session::SessionComponent, index: usize) -> String {
    session
        .with_store(|store| store.line()[index].clone())
        .expect("lock")
        .expect("an id")
}

/// Three turns: sys, q1, a1, q2, a2, q3, a3.
fn three_turns(agent: &mut aphid_agent::Agent, session: &session::SessionComponent) {
    for turn in 1..=3 {
        agent.transcript_mut().push_user(&format!("q{turn}"));
        agent.transcript_mut().commit(answer(&format!("a{turn}")));
    }
    flush(agent, session);
}

#[test]
fn a_fork_at_a_prompt_starts_before_it_and_gives_it_back() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    three_turns(&mut agent, &session);
    let q2 = node(&session, 3);

    let done = session::checkout(&session, &mut agent, &q2, session::Move::Fork).expect("fork");
    assert_eq!(done.prefill.as_deref(), Some("q2"));
    assert_eq!(texts(agent.transcript()), ["sys", "q1", "a1"]);

    agent.transcript_mut().push_user("q2 edited");
    flush(&agent, &session);
    let path = session.path().expect("path");
    let (_, head) = session::load(&path).expect("load");
    assert_eq!(texts(&head), ["sys", "q1", "a1", "q2 edited"]);
    let view = session::Tree::read(&path).expect("tree").view();
    assert_eq!(view.turns.len(), 4);
}

#[test]
fn a_fork_at_an_answer_continues_after_it() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    three_turns(&mut agent, &session);
    let a1 = node(&session, 2);

    let done = session::checkout(&session, &mut agent, &a1, session::Move::Fork).expect("fork");
    assert_eq!(done.prefill, None);
    assert_eq!(done.head.as_deref(), Some(a1.as_str()));
    assert_eq!(texts(agent.transcript()), ["sys", "q1", "a1"]);
}

#[test]
fn a_fork_inside_a_turn_is_refused() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    agent.transcript_mut().push_user("q");
    let mut call = MessageBuffer::new(AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "m",
    ));
    let index = call.begin_tool_call("c1", "read");
    call.push_delta(index, "{}");
    agent.transcript_mut().commit(call);
    agent.transcript_mut().push_tool_result(
        ToolResultMeta::new("c1", "read"),
        &[ContentInput::Text("x")],
    );
    flush(&agent, &session);

    for index in [2, 3] {
        let inside = node(&session, index);
        assert!(session::checkout(&session, &mut agent, &inside, session::Move::Fork).is_err());
    }
    assert_eq!(agent.transcript().len(), 4, "nothing moved");
}

#[test]
fn a_jump_to_another_branch_replays_it_and_continues_there() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    three_turns(&mut agent, &session);
    let q2 = node(&session, 3);
    let a3 = node(&session, 6);

    session::checkout(&session, &mut agent, &q2, session::Move::Fork).expect("fork");
    agent.transcript_mut().push_user("q2b");
    agent.transcript_mut().commit(answer("a2b"));
    flush(&agent, &session);

    // Back to the first branch, from its second prompt: the newest leaf under
    // it is the end of the first branch.
    let done = session::checkout(&session, &mut agent, &q2, session::Move::Jump).expect("jump");
    assert_eq!(done.head.as_deref(), Some(a3.as_str()));
    assert_eq!(
        texts(agent.transcript()),
        ["sys", "q1", "a1", "q2", "a2", "q3", "a3"]
    );

    agent.transcript_mut().push_user("q4");
    flush(&agent, &session);
    let path = session.path().expect("path");
    let tree = session::Tree::read(&path).expect("tree");
    let q4 = tree.nodes.last().expect("q4");
    assert_eq!(q4.parent.as_deref(), Some(a3.as_str()));
    let (_, head) = session::load(&path).expect("load");
    assert_eq!(texts(&head).last().map(String::as_str), Some("q4"));
}

#[test]
fn a_resume_that_drops_system_notes_still_appends_after_the_right_message() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    agent.transcript_mut().push_user("q1");
    agent.transcript_mut().push_system("a note from the middle");
    agent.transcript_mut().commit(answer("a1"));
    flush(&agent, &session);
    let a1 = node(&session, 3);
    let path = session.path().expect("path");

    // Resume as a front end does: a fresh agent, then splice.
    let listeners = std::sync::Arc::new(aphid_agent::TranscriptListeners::default());
    let (resumed, restored) = session::attach(
        &temp.root,
        &temp.root,
        &temp.root,
        None,
        Some(&session::Resume::head(path.clone())),
        listeners,
    )
    .expect("attach");
    let restored = restored.expect("a transcript");
    let mut fresh = aphid_agent::Agent::builder()
        .model(aphid_core::providers::deepseek::flash())
        .system("today's sys")
        .build();
    assert_eq!(session::splice(&mut fresh, &restored, Some(&resumed)), 2);
    assert_eq!(texts(fresh.transcript()), ["today's sys", "q1", "a1"]);

    fresh.transcript_mut().push_user("q2");
    flush(&fresh, &resumed);
    let tree = session::Tree::read(&path).expect("tree");
    assert_eq!(tree.nodes.len(), 5, "today's system prompt is not written");
    assert_eq!(tree.nodes[4].parent.as_deref(), Some(a1.as_str()));

    // And a fork at the first prompt hangs from the saved system prompt.
    let q1 = node(&resumed, 1);
    session::checkout(&resumed, &mut fresh, &q1, session::Move::Fork).expect("fork");
    assert_eq!(texts(fresh.transcript()), ["today's sys"]);
    fresh.transcript_mut().push_user("q1b");
    flush(&fresh, &resumed);
    let tree = session::Tree::read(&path).expect("tree");
    assert_eq!(
        tree.nodes.last().and_then(|n| n.parent.clone()),
        Some(tree.nodes[0].id.clone())
    );
}

#[test]
fn starting_a_new_session_writes_a_new_file() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    three_turns(&mut agent, &session);
    let before = session.path().expect("path");

    session::start(&session, &mut agent, &temp.root, &temp.root, &temp.root).expect("start");
    assert_eq!(texts(agent.transcript()), ["sys"]);
    agent.transcript_mut().push_user("fresh");
    flush(&agent, &session);

    let after = session.path().expect("path");
    assert_ne!(before, after);
    let (_, fresh) = session::load(&after).expect("load");
    assert_eq!(texts(&fresh), ["sys", "fresh"], "the first prompt is kept");
    let (_, old) = session::load(&before).expect("load");
    assert_eq!(old.len(), 7, "the old session is untouched");
}

#[test]
fn renaming_names_the_branch_the_session_is_on() {
    let temp = Temp::new();
    let (mut agent, session) = agent_with_session(&temp);
    three_turns(&mut agent, &session);
    session::rename(&session, None, "first try").expect("rename");
    let path = session.path().expect("path");
    assert_eq!(
        session::Tree::read(&path).expect("tree").title(),
        "first try"
    );

    let a1 = node(&session, 2);
    session::checkout(&session, &mut agent, &a1, session::Move::Fork).expect("fork");
    agent.transcript_mut().push_user("other way");
    flush(&agent, &session);
    session::rename(&session, None, "second try").expect("rename");

    let view = session::Tree::read(&path).expect("tree").view();
    assert_eq!(view.title, "first try");
    let branch = view
        .turns
        .iter()
        .find(|turn| turn.prompt == "other way")
        .expect("branch");
    assert_eq!(branch.label.as_deref(), Some("second try"));
}
