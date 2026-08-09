//! Request encoding for the OpenAI Chat Completions protocol.

use aphid_core::providers::deepseek;
use aphid_core::{
    Api, AssistantMeta, Compat, ContentInput, Error, Json, MessageBuffer, Model,
    OpenAiCompletionsCompat, ProviderId, SimpleStreamOptions, StopReason, ThinkingLevel, Tool,
    ToolResultMeta, Transcript, encode_request,
};

fn body(transcript: &Transcript, tools: &[Tool], options: &SimpleStreamOptions) -> Json {
    let encoded = encode_request(&deepseek::flash(), transcript, tools, options).unwrap();
    serde_json::from_str(&encoded).expect("the encoder emits valid JSON")
}

fn conversation() -> Transcript {
    let mut t = Transcript::new();
    t.push_system("You are terse.");
    t.push_user("weather in Lisbon?");

    let mut turn = MessageBuffer::new(AssistantMeta::new(
        Api::OpenAiCompletions,
        ProviderId::DEEPSEEK,
        "deepseek-v4-flash",
    ));
    let thinking = turn.begin_thinking();
    turn.push_delta(thinking, "they want weather");
    let call = turn.begin_tool_call("call_a", "get_weather");
    turn.push_delta(call, r#"{"city":"Lisbon"}"#);
    turn.meta_mut().stop_reason = StopReason::ToolUse;
    t.commit(turn);

    t.push_tool_result(
        ToolResultMeta::new("call_a", "get_weather"),
        &[ContentInput::Text("18C, clear")],
    );
    t
}

#[test]
fn a_whole_conversation_round_trips_into_the_wire_shape() {
    let json = body(&conversation(), &[], &SimpleStreamOptions::default());
    let messages = json["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);

    // DeepSeek does not take the `developer` role.
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are terse.");
    assert_eq!(messages[1]["role"], "user");

    let assistant = &messages[2];
    assert_eq!(assistant["role"], "assistant");
    let call = &assistant["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["id"], "call_a");
    assert_eq!(call["function"]["name"], "get_weather");
    // Arguments replay byte-identically, as the string the model produced.
    assert_eq!(call["function"]["arguments"], r#"{"city":"Lisbon"}"#);

    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_a");
    assert_eq!(messages[3]["content"], "18C, clear");
}

#[test]
fn deepseek_gets_max_tokens_and_streamed_usage() {
    let json = body(&conversation(), &[], &SimpleStreamOptions::default());
    assert_eq!(json["stream"], true);
    assert_eq!(json["stream_options"]["include_usage"], true);
    assert_eq!(json["max_tokens"], deepseek::MAX_OUTPUT_TOKENS);
    assert!(json.get("max_completion_tokens").is_none());
}

#[test]
fn thinking_is_explicitly_disabled_when_no_level_is_asked_for() {
    let json = body(&conversation(), &[], &SimpleStreamOptions::default());
    assert_eq!(json["thinking"]["type"], "disabled");
    assert!(json.get("reasoning_effort").is_none());
    assert!(
        json["messages"][2].get("reasoning_content").is_none(),
        "reasoning_content is only required once thinking is on"
    );
}

#[test]
fn asking_for_a_level_enables_thinking_and_maps_the_effort() {
    let options = SimpleStreamOptions {
        reasoning: Some(ThinkingLevel::XHigh),
        ..Default::default()
    };
    let json = body(&conversation(), &[], &options);
    assert_eq!(json["thinking"]["type"], "enabled");
    // xhigh folds onto DeepSeek's `max`.
    assert_eq!(json["reasoning_effort"], "max");
    assert_eq!(
        json["messages"][2]["reasoning_content"],
        "they want weather"
    );
}

#[test]
fn temperature_is_dropped_in_thinking_mode_because_deepseek_rejects_it() {
    let mut options = SimpleStreamOptions::default();
    options.stream.temperature = Some(0.4);
    let json = body(&conversation(), &[], &options);
    assert_eq!(json["temperature"], 0.4);

    options.reasoning = Some(ThinkingLevel::High);
    let json = body(&conversation(), &[], &options);
    assert!(json.get("temperature").is_none());
}

#[test]
fn tools_are_emitted_with_their_schema_verbatim() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "city": { "type": "string" } },
        "required": ["city"]
    });
    let tools = vec![Tool::new("get_weather", "Look up weather.", schema.clone())];
    let json = body(&conversation(), &tools, &SimpleStreamOptions::default());

    let function = &json["tools"][0]["function"];
    assert_eq!(json["tools"][0]["type"], "function");
    assert_eq!(function["name"], "get_weather");
    assert_eq!(function["description"], "Look up weather.");
    assert_eq!(function["parameters"], schema);
    assert!(
        function.get("strict").is_none(),
        "strict is opt-in per tool"
    );
}

#[test]
fn constrained_tools_ask_for_strict_mode() {
    let tool = Tool::new("t", "d", serde_json::json!({"type": "object"})).constrained(
        aphid_core::ConstrainedSampling::JsonSchema {
            strict: aphid_core::Strictness::Require,
        },
    );
    let json = body(&conversation(), &[tool], &SimpleStreamOptions::default());
    assert_eq!(json["tools"][0]["function"]["strict"], true);
}

#[test]
fn sampling_passthrough_overrides_everything_before_it() {
    let mut options = SimpleStreamOptions::default();
    options.stream.max_tokens = Some(100);
    options.stream.sampling_params = Some(serde_json::json!({ "top_p": 0.9, "max_tokens": 42 }));
    let json = body(&conversation(), &[], &options);
    assert_eq!(json["top_p"], 0.9);
    assert_eq!(json["max_tokens"], 42);
}

#[test]
fn an_endpoint_that_wants_the_developer_role_gets_it() {
    let mut model = deepseek::flash();
    model.compat = Compat::from(OpenAiCompletionsCompat::default());
    let encoded = encode_request(
        &model,
        &conversation(),
        &[],
        &SimpleStreamOptions::default(),
    )
    .unwrap();
    let json: Json = serde_json::from_str(&encoded).unwrap();
    assert_eq!(json["messages"][0]["role"], "developer");
    assert_eq!(json["max_completion_tokens"], deepseek::MAX_OUTPUT_TOKENS);
}

#[test]
fn images_are_rejected_rather_than_silently_dropped() {
    let mut t = Transcript::new();
    t.push_user_parts(&[
        ContentInput::Text("what is this"),
        ContentInput::Image {
            data: &[1, 2, 3],
            mime: "image/png",
        },
    ]);
    let error = encode_request(&deepseek::flash(), &t, &[], &SimpleStreamOptions::default())
        .expect_err("this protocol cannot carry images");
    assert!(matches!(error, Error::UnsupportedContent("image")));
}

#[test]
fn text_needing_escapes_survives_the_hand_written_encoder() {
    let mut t = Transcript::new();
    let awkward = "quotes \" backslash \\ newline \n tab \t unicode 世界 🦀";
    t.push_user(awkward);
    let json = body(&t, &[], &SimpleStreamOptions::default());
    assert_eq!(json["messages"][0]["content"], awkward);
}

#[test]
fn a_non_reasoning_model_gets_no_thinking_field_at_all() {
    let mut model: Model = deepseek::flash();
    model.reasoning = false;
    let encoded = encode_request(
        &model,
        &conversation(),
        &[],
        &SimpleStreamOptions::default(),
    )
    .unwrap();
    let json: Json = serde_json::from_str(&encoded).unwrap();
    assert!(json.get("thinking").is_none());
}
