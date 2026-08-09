//! Behaviour of the arena-backed conversation store, through its public API.

use aphid_core::{
    Api, AssistantMeta, ContentInput, ContentRef, MessageBuffer, ProviderId, Role, StopReason,
    ToolResultMeta, Transcript,
};

fn assistant_meta() -> AssistantMeta {
    AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "deepseek-v4-flash",
    )
}

/// A four-message conversation covering every role and content kind.
fn sample() -> Transcript {
    let mut t = Transcript::new();
    t.push_system("You are terse.");
    t.push_user_parts(&[
        ContentInput::Text("look at this"),
        ContentInput::Image {
            data: &[0xff, 0xd8, 0xff],
            mime: "image/jpeg",
        },
    ]);

    let mut turn = MessageBuffer::new(assistant_meta());
    let thinking = turn.begin_thinking();
    turn.push_delta(thinking, "the user wants a tool");
    turn.set_signature(thinking, "sig-abc");
    let text = turn.begin_text();
    turn.push_delta(text, "Calling out.");
    let call = turn.begin_tool_call("call_1", "calculator");
    turn.push_delta(call, r#"{"expr":"2+2"}"#);
    turn.meta_mut().stop_reason = StopReason::ToolUse;
    t.commit(turn);

    t.push_tool_result(
        ToolResultMeta::new("call_1", "calculator"),
        &[ContentInput::Text("4")],
    );
    t
}

#[test]
fn the_system_prompt_is_just_a_message() {
    let t = sample();
    let system = t.get(0).unwrap();
    assert_eq!(system.role(), Role::System);
    assert_eq!(
        system.content().next().unwrap().text(),
        Some("You are terse.")
    );
    assert!(system.assistant().is_none());
    // It participates in iteration like anything else.
    assert_eq!(t.iter().filter(|m| m.role() == Role::System).count(), 1);
}

#[test]
fn every_content_kind_reads_back_through_the_view_layer() {
    let t = sample();

    let user = t.get(1).unwrap();
    let mut parts = user.content();
    assert_eq!(parts.next().unwrap().text(), Some("look at this"));
    let ContentRef::Image(image) = parts.next().unwrap() else {
        panic!("expected an image")
    };
    assert_eq!(image.mime(), "image/jpeg");
    assert_eq!(image.data(), &[0xff, 0xd8, 0xff]);

    let assistant = t.get(2).unwrap();
    let mut blocks = assistant.content();
    let ContentRef::Thinking(thinking) = blocks.next().unwrap() else {
        panic!("expected thinking")
    };
    assert_eq!(thinking.text(), "the user wants a tool");
    assert_eq!(thinking.signature(), Some("sig-abc"));
    assert!(!thinking.redacted());
    assert_eq!(blocks.next().unwrap().text(), Some("Calling out."));
    let ContentRef::ToolCall(call) = blocks.next().unwrap() else {
        panic!("expected a tool call")
    };
    assert_eq!(call.id(), "call_1");
    assert_eq!(call.arguments_raw(), r#"{"expr":"2+2"}"#);
    assert_eq!(call.arguments().unwrap()["expr"], "2+2");
    assert_eq!(call.thought_signature(), None);
    assert_eq!(
        assistant.assistant().unwrap().stop_reason,
        StopReason::ToolUse
    );

    let result = t.get(3).unwrap();
    assert_eq!(result.tool_result().unwrap().tool_call_id, "call_1");
    assert!(!result.tool_result().unwrap().is_error);
}

#[test]
fn absent_signatures_read_as_none_not_empty_strings() {
    let mut t = Transcript::new();
    t.push_user("hi");
    let ContentRef::Text(text) = t.get(0).unwrap().content().next().unwrap() else {
        panic!("expected text");
    };
    assert_eq!(text.signature(), None);
}

#[test]
fn malformed_tool_arguments_surface_as_an_error() {
    let mut t = Transcript::new();
    let mut turn = MessageBuffer::new(assistant_meta());
    let call = turn.begin_tool_call("call_1", "calculator");
    turn.push_delta(call, "{not json");
    t.commit(turn);

    let ContentRef::ToolCall(call) = t.get(0).unwrap().content().next().unwrap() else {
        panic!("expected a tool call");
    };
    // The raw text is still available even though it does not parse.
    assert_eq!(call.arguments_raw(), "{not json");
    assert!(call.arguments().is_err());
}

#[test]
fn truncate_rewinds_the_arena_exactly() {
    let mut t = sample();
    let before = t.arena_stats();
    let kept_messages = 2;

    let checkpoint = {
        let mut probe = Transcript::new();
        probe.push_system("You are terse.");
        probe.push_user_parts(&[
            ContentInput::Text("look at this"),
            ContentInput::Image {
                data: &[0xff, 0xd8, 0xff],
                mime: "image/jpeg",
            },
        ]);
        probe.arena_stats()
    };

    t.truncate(kept_messages);
    let after = t.arena_stats();

    assert_eq!(t.len(), kept_messages);
    assert!(after.text_bytes < before.text_bytes);
    // Removing a suffix leaves nothing behind: the arena matches a transcript
    // that only ever held those two messages.
    assert_eq!(after.text_bytes, checkpoint.text_bytes);
    assert_eq!(after.blob_bytes, checkpoint.blob_bytes);
    assert_eq!(after.text_garbage_bytes(), 0);
    assert_eq!(after.tool_calls, 0);
}

#[test]
fn truncate_past_the_end_is_a_no_op() {
    let mut t = sample();
    let before = t.arena_stats();
    t.truncate(99);
    assert_eq!(t.arena_stats(), before);
}

#[test]
fn compact_into_drops_garbage_and_preserves_content() {
    let mut t = sample();
    // Overwrite a block's text by relocating it, which strands the old bytes.
    let mut turn = MessageBuffer::new(assistant_meta());
    let a = turn.begin_text();
    let b = turn.begin_text();
    turn.push_delta(a, "first");
    turn.push_delta(b, "second");
    turn.push_delta(a, "-again"); // forces `a` to relocate, stranding "first"
    t.commit(turn);
    assert!(t.arena_stats().text_garbage_bytes() > 0);

    let keep: Vec<_> = (0..t.len()).filter_map(|i| t.id_at(i)).collect();
    let mut compacted = Transcript::new();
    t.compact_into(&keep, &mut compacted);

    assert_eq!(compacted.len(), t.len());
    assert_eq!(compacted.arena_stats().text_garbage_bytes(), 0);
    for (original, copy) in t.iter().zip(compacted.iter()) {
        assert_eq!(original.role(), copy.role());
        let a: Vec<_> = original.content().filter_map(|c| c.text()).collect();
        let b: Vec<_> = copy.content().filter_map(|c| c.text()).collect();
        assert_eq!(a, b);
    }
}

#[test]
fn compact_into_can_drop_messages_from_the_middle() {
    let t = sample();
    let keep = vec![t.id_at(0).unwrap(), t.id_at(3).unwrap()];
    let mut compacted = Transcript::new();
    t.compact_into(&keep, &mut compacted);

    assert_eq!(compacted.len(), 2);
    assert_eq!(compacted.get(0).unwrap().role(), Role::System);
    assert_eq!(compacted.get(1).unwrap().role(), Role::ToolResult);
    assert_eq!(
        compacted.get(1).unwrap().tool_result().unwrap().tool_name,
        "calculator"
    );
    assert_eq!(compacted.arena_stats().blob_bytes, 0);
}

// The generation tag is debug-only; release builds trade the check for the
// space it occupies in every `MessageId`.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "different transcript")]
fn an_id_from_another_transcript_is_rejected() {
    let a = sample();
    let b = sample();
    let _ = b.message(a.id_at(0).unwrap());
}

#[test]
fn last_and_get_agree() {
    let t = sample();
    assert_eq!(t.last().unwrap().role(), Role::ToolResult);
    assert!(Transcript::new().last().is_none());
    assert!(t.get(99).is_none());
    assert!(t.try_get(99).is_err());
}
