//! The OpenAI Chat Completions protocol: request encoding and chunk decoding.
//!
//! Encoding is hand-written and reads straight from the transcript arena.
//! Decoding goes through serde into *borrowed* structs, so a chunk is parsed
//! without copying the strings out of the response buffer — they are copied
//! exactly once, when appended to the message arena.

use std::borrow::Cow;
use std::collections::VecDeque;

use crate::api::json_writer::JsonWriter;
use crate::compat::{OpenAiCompletionsCompat, ThinkingFormat};
use crate::content::BlockKind;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::json::Json;
use crate::message::StopReason;
use crate::model::Model;
use crate::options::SimpleStreamOptions;
use crate::thinking::ModelThinkingLevel;
use crate::tool::{ConstrainedSampling, Tool};
use crate::transcript::Transcript;
use crate::view::{ContentRef, MessageRef};
use crate::{MessageBuffer, Role, Usage};

/// The path appended to a provider's base URL.
pub const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Sentinel payload the protocol uses to close a stream.
const DONE: &str = "[DONE]";

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Serialize a request body for `model` from the whole transcript.
///
/// # Errors
/// Returns [`Error::UnsupportedContent`] for content this protocol cannot carry.
pub fn encode_request(
    model: &Model,
    transcript: &Transcript,
    tools: &[Tool],
    options: &SimpleStreamOptions,
) -> Result<String> {
    let fallback = OpenAiCompletionsCompat::default();
    let compat = model.compat.openai_completions().unwrap_or(&fallback);
    let thinking_on = model.reasoning && options.reasoning.is_some();

    // A rough guess at the body size; the arena's live bytes dominate it.
    let mut w = JsonWriter::with_capacity(transcript.arena_stats().live_text_bytes as usize + 1024);
    w.begin_object();
    w.field_str("model", &model.id);
    w.field_bool("stream", true);

    if compat.supports_usage_in_streaming {
        w.key("stream_options");
        w.begin_object();
        w.field_bool("include_usage", true);
        w.end_object();
    }

    w.key("messages");
    w.begin_array();
    for message in transcript.iter() {
        encode_message(&mut w, message, compat, thinking_on)?;
    }
    w.end_array();

    if !tools.is_empty() {
        w.key("tools");
        w.begin_array();
        for tool in tools {
            encode_tool(&mut w, tool, compat);
        }
        w.end_array();
    }

    let max_tokens = options.stream.max_tokens.unwrap_or(model.max_tokens);
    w.field_u32(compat.max_tokens_field.as_str(), max_tokens);

    if let Some(temperature) = options.stream.temperature
        && (!thinking_on || compat.supports_temperature_while_thinking)
    {
        w.field_f32("temperature", temperature);
    }

    encode_thinking(&mut w, model, compat, options);

    // Caller-supplied passthrough goes last so it can override anything above.
    if let Some(Json::Object(extra)) = &options.stream.sampling_params {
        for (key, value) in extra {
            w.field_raw(key, &value.to_string());
        }
    }

    w.end_object();
    Ok(w.finish())
}

fn encode_message(
    w: &mut JsonWriter,
    message: MessageRef<'_>,
    compat: &OpenAiCompletionsCompat,
    thinking_on: bool,
) -> Result<()> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for block in message.content() {
        match block {
            ContentRef::Text(t) => text.push_str(t.text()),
            ContentRef::Thinking(t) => reasoning.push_str(t.text()),
            ContentRef::ToolCall(c) => tool_calls.push(c),
            ContentRef::Image(_) => return Err(Error::UnsupportedContent("image")),
        }
    }

    w.begin_object();
    match message.role() {
        Role::System => {
            let role = if compat.supports_developer_role {
                "developer"
            } else {
                "system"
            };
            w.field_str("role", role);
            w.field_str("content", &text);
        }
        Role::User => {
            w.field_str("role", "user");
            w.field_str("content", &text);
        }
        Role::Assistant => {
            w.field_str("role", "assistant");
            // Assistant content is always a plain string: sending it as an array
            // of blocks makes some models mirror the structure back verbatim.
            if !text.is_empty() || tool_calls.is_empty() {
                w.field_str("content", &text);
            }
            if thinking_on && compat.requires_reasoning_content_on_assistant_messages {
                w.field_str("reasoning_content", &reasoning);
            }
            if !tool_calls.is_empty() {
                w.key("tool_calls");
                w.begin_array();
                for call in tool_calls {
                    w.begin_object();
                    w.field_str("id", call.id());
                    w.field_str("type", "function");
                    w.key("function");
                    w.begin_object();
                    w.field_str("name", call.name());
                    // Replayed byte-identical, straight out of the arena.
                    w.field_str("arguments", call.arguments_raw());
                    w.end_object();
                    w.end_object();
                }
                w.end_array();
            }
        }
        Role::ToolResult => {
            let meta = message
                .tool_result()
                .expect("tool result carries its metadata");
            w.field_str("role", "tool");
            w.field_str("tool_call_id", &meta.tool_call_id);
            if compat.requires_tool_result_name {
                w.field_str("name", &meta.tool_name);
            }
            w.field_str("content", &text);
        }
    }
    w.end_object();
    Ok(())
}

fn encode_tool(w: &mut JsonWriter, tool: &Tool, compat: &OpenAiCompletionsCompat) {
    w.begin_object();
    w.field_str("type", "function");
    w.key("function");
    w.begin_object();
    w.field_str("name", &tool.name);
    w.field_str("description", &tool.description);
    w.field_raw("parameters", &tool.parameters.to_string());
    if compat.supports_strict_mode
        && matches!(
            tool.constrained_sampling,
            Some(ConstrainedSampling::JsonSchema { .. })
        )
    {
        w.field_bool("strict", true);
    }
    w.end_object();
    w.end_object();
}

fn encode_thinking(
    w: &mut JsonWriter,
    model: &Model,
    compat: &OpenAiCompletionsCompat,
    options: &SimpleStreamOptions,
) {
    if !model.reasoning {
        return;
    }
    match compat.thinking_format {
        ThinkingFormat::DeepSeek => {
            w.key("thinking");
            w.begin_object();
            w.field_str(
                "type",
                if options.reasoning.is_some() {
                    "enabled"
                } else {
                    "disabled"
                },
            );
            w.end_object();
            if let Some(level) = options.reasoning
                && compat.supports_reasoning_effort
                && let Some(effort) = model
                    .thinking_levels
                    .resolve(ModelThinkingLevel::Level(level))
            {
                w.field_str("reasoning_effort", effort);
            }
        }
        ThinkingFormat::OpenAi => {
            if let Some(level) = options.reasoning
                && compat.supports_reasoning_effort
                && let Some(effort) = model
                    .thinking_levels
                    .resolve(ModelThinkingLevel::Level(level))
            {
                w.field_str("reasoning_effort", effort);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (decode side)
// ---------------------------------------------------------------------------

/// `Cow` rather than `&str` so chunks containing JSON escapes still decode
/// without copying the ones that do not.
#[derive(serde::Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(default)]
    choices: Vec<Choice<'a>>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(serde::Deserialize)]
struct Choice<'a> {
    #[serde(default, borrow)]
    delta: Delta<'a>,
    #[serde(default, borrow)]
    finish_reason: Option<Cow<'a, str>>,
}

#[derive(serde::Deserialize, Default)]
struct Delta<'a> {
    #[serde(default, borrow)]
    content: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    reasoning_content: Option<Cow<'a, str>>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta<'a>>,
}

#[derive(serde::Deserialize)]
struct ToolCallDelta<'a> {
    #[serde(default)]
    index: u32,
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default)]
    function: Option<FunctionDelta<'a>>,
}

#[derive(serde::Deserialize)]
struct FunctionDelta<'a> {
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    arguments: Option<Cow<'a, str>>,
}

#[derive(serde::Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u64,
    /// DeepSeek's cache accounting.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
}

#[derive(serde::Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(serde::Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

impl WireUsage {
    fn into_usage(self, model: &Model) -> Usage {
        let cache_read = self
            .prompt_cache_hit_tokens
            .or_else(|| {
                self.prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
            })
            .unwrap_or(0);
        let mut usage = Usage {
            // `prompt_tokens` counts cached reads too; keep the buckets disjoint.
            input: self.prompt_tokens.saturating_sub(cache_read),
            output: self.completion_tokens,
            cache_read,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: self
                .completion_tokens_details
                .and_then(|d| d.reasoning_tokens),
            total_tokens: self.total_tokens,
            cost: crate::Cost::default(),
        };
        usage.cost = model.cost.cost_of(&usage);
        usage
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Turns SSE payloads into [`Event`]s, writing content into a [`MessageBuffer`]
/// as it goes.
#[derive(Debug, Default)]
pub(crate) struct ChunkDecoder {
    text_block: Option<u32>,
    thinking_block: Option<u32>,
    /// Wire `tool_calls[].index` to the block index it was opened as.
    tool_blocks: Vec<(u32, u32)>,
    open: Vec<u32>,
    /// Recorded on `finish_reason`, but not emitted until the stream closes, so
    /// the trailing usage-only chunk is still accounted for.
    stop: Option<StopReason>,
    seen_done: bool,
}

impl ChunkDecoder {
    /// Whether the provider sent its end-of-stream sentinel.
    pub(crate) fn saw_done(&self) -> bool {
        self.seen_done
    }

    /// The stop reason reported so far, defaulting to a clean stop.
    pub(crate) fn stop_reason(&self) -> StopReason {
        self.stop.unwrap_or(StopReason::Stop)
    }

    /// Apply one SSE payload.
    ///
    /// # Errors
    /// Returns [`Error::Json`] when a chunk is not valid JSON.
    pub(crate) fn apply(
        &mut self,
        payload: &str,
        buffer: &mut MessageBuffer,
        model: &Model,
        out: &mut VecDeque<Event>,
    ) -> Result<()> {
        if payload.trim() == DONE {
            self.seen_done = true;
            return Ok(());
        }

        let chunk: Chunk<'_> = serde_json::from_str(payload)?;

        if let Some(id) = chunk.id
            && buffer.meta().response_id.is_none()
        {
            buffer.meta_mut().response_id = Some(id.as_ref().into());
        }
        if let Some(reported) = chunk.model
            && reported != buffer.meta().model
        {
            buffer.meta_mut().response_model = Some(reported.as_ref().into());
        }
        if let Some(usage) = chunk.usage {
            buffer.meta_mut().usage = usage.into_usage(model);
        }

        for choice in chunk.choices {
            if let Some(reasoning) = choice.delta.reasoning_content.filter(|r| !r.is_empty()) {
                let index = self.thinking_block(buffer, out);
                let span = buffer.push_delta(index, &reasoning);
                out.push_back(Event::Delta {
                    index,
                    kind: BlockKind::Thinking,
                    span,
                });
            }
            if let Some(content) = choice.delta.content.filter(|c| !c.is_empty()) {
                let index = self.text_block(buffer, out);
                let span = buffer.push_delta(index, &content);
                out.push_back(Event::Delta {
                    index,
                    kind: BlockKind::Text,
                    span,
                });
            }
            for call in choice.delta.tool_calls {
                self.apply_tool_call(call, buffer, out);
            }
            if let Some(reason) = choice.finish_reason {
                self.stop = Some(match reason.as_ref() {
                    "stop" => StopReason::Stop,
                    "length" => StopReason::Length,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    _ => StopReason::Stop,
                });
                buffer.meta_mut().raw_stop_reason = Some(reason.as_ref().into());
                buffer.meta_mut().end_turn = Some(reason == "stop");
            }
        }
        Ok(())
    }

    /// Close every block still open, in the order they were opened.
    pub(crate) fn close_open_blocks(&mut self, out: &mut VecDeque<Event>) {
        for index in self.open.drain(..) {
            out.push_back(Event::BlockEnd { index });
        }
    }

    fn apply_tool_call(
        &mut self,
        call: ToolCallDelta<'_>,
        buffer: &mut MessageBuffer,
        out: &mut VecDeque<Event>,
    ) {
        let existing = self
            .tool_blocks
            .iter()
            .find(|(wire, _)| *wire == call.index)
            .map(|(_, b)| *b);
        let index = match existing {
            Some(index) => index,
            None => {
                // Prose is finished once the model starts calling tools.
                self.close_prose(out);
                // A tool call is only identifiable once its id and name arrive,
                // which the protocol guarantees on the first delta for an index.
                let id = call.id.as_deref().unwrap_or_default();
                let name = call
                    .function
                    .as_ref()
                    .and_then(|f| f.name.as_deref())
                    .unwrap_or_default();
                let index = buffer.begin_tool_call(id, name);
                self.tool_blocks.push((call.index, index));
                self.open.push(index);
                out.push_back(Event::BlockStart {
                    index,
                    kind: BlockKind::ToolCall,
                });
                index
            }
        };
        if let Some(arguments) = call
            .function
            .and_then(|f| f.arguments)
            .filter(|a| !a.is_empty())
        {
            let span = buffer.push_delta(index, &arguments);
            out.push_back(Event::Delta {
                index,
                kind: BlockKind::ToolCall,
                span,
            });
        }
    }

    /// Close the thinking and text blocks, if either is open.
    fn close_prose(&mut self, out: &mut VecDeque<Event>) {
        let thinking = self.thinking_block.take();
        self.close_block(thinking, out);
        let text = self.text_block.take();
        self.close_block(text, out);
    }

    fn close_block(&mut self, block: Option<u32>, out: &mut VecDeque<Event>) {
        let Some(index) = block else { return };
        if let Some(pos) = self.open.iter().position(|i| *i == index) {
            self.open.remove(pos);
            out.push_back(Event::BlockEnd { index });
        }
    }

    fn thinking_block(&mut self, buffer: &mut MessageBuffer, out: &mut VecDeque<Event>) -> u32 {
        if let Some(index) = self.thinking_block {
            return index;
        }
        let index = buffer.begin_thinking();
        self.thinking_block = Some(index);
        self.open.push(index);
        out.push_back(Event::BlockStart {
            index,
            kind: BlockKind::Thinking,
        });
        index
    }

    fn text_block(&mut self, buffer: &mut MessageBuffer, out: &mut VecDeque<Event>) -> u32 {
        if let Some(index) = self.text_block {
            return index;
        }
        // Reasoning is complete once prose starts.
        let thinking = self.thinking_block.take();
        self.close_block(thinking, out);
        let index = buffer.begin_text();
        self.text_block = Some(index);
        self.open.push(index);
        out.push_back(Event::BlockStart {
            index,
            kind: BlockKind::Text,
        });
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::deepseek;

    fn decode(payloads: &[&str]) -> (MessageBuffer, Vec<Event>) {
        let model = deepseek::flash();
        let meta =
            crate::AssistantMeta::new(model.api.clone(), model.provider.clone(), model.id.clone());
        let mut buffer = MessageBuffer::new(meta);
        let mut decoder = ChunkDecoder::default();
        let mut events = VecDeque::new();
        for payload in payloads {
            decoder
                .apply(payload, &mut buffer, &model, &mut events)
                .unwrap();
        }
        decoder.close_open_blocks(&mut events);
        buffer.meta_mut().stop_reason = decoder.stop_reason();
        (buffer, events.into_iter().collect())
    }

    #[test]
    fn reasoning_then_content_opens_two_blocks_in_order() {
        let (buffer, events) = decode(&[
            r#"{"id":"resp-1","model":"deepseek-v4-flash","choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":" ok"}}]}"#,
            r#"{"choices":[{"delta":{"content":"4"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);

        let kinds: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { index, kind } => Some((*index, *kind)),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, [(0, BlockKind::Thinking), (1, BlockKind::Text)]);
        // Reasoning is closed as soon as prose begins.
        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::BlockEnd { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [0, 1]);

        let partial = buffer.partial();
        let texts: Vec<_> = partial.content().filter_map(|c| c.text()).collect();
        assert_eq!(texts, ["hmm ok", "4"]);
        assert_eq!(buffer.meta().stop_reason, StopReason::Stop);
        assert_eq!(buffer.meta().response_id.as_deref(), Some("resp-1"));
        assert_eq!(buffer.meta().raw_stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn delta_spans_name_only_the_new_bytes() {
        let (buffer, events) = decode(&[
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
        ]);
        let resolved: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Delta { span, .. } => Some(buffer.text(*span).to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(resolved, ["Hel", "lo"]);
    }

    #[test]
    fn tool_call_arguments_accumulate_as_raw_json() {
        let (buffer, _) = decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Lisbon\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);

        assert_eq!(buffer.meta().stop_reason, StopReason::ToolUse);
        let partial = buffer.partial();
        let ContentRef::ToolCall(call) = partial.content().next().unwrap() else {
            panic!("expected a tool call");
        };
        assert_eq!(call.id(), "call_a");
        assert_eq!(call.name(), "get_weather");
        assert_eq!(call.arguments_raw(), r#"{"city":"Lisbon"}"#);
        assert_eq!(call.arguments().unwrap()["city"], "Lisbon");
    }

    #[test]
    fn a_tool_call_closes_the_prose_blocks_before_it() {
        // Regression: reasoning and text used to stay open when tool calls
        // began, so their BlockEnds arrived after the tool call's BlockStart.
        let (_, events) = decode(&[
            r#"{"choices":[{"delta":{"reasoning_content":"they want weather"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Looking that up."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"w","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);

        let structure: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::BlockStart { index, .. } => Some(format!("start {index}")),
                Event::BlockEnd { index } => Some(format!("end {index}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            structure,
            ["start 0", "end 0", "start 1", "end 1", "start 2", "end 2"],
            "every block closes before the next one opens"
        );
    }

    #[test]
    fn parallel_tool_calls_stay_on_their_own_blocks() {
        let (buffer, _) = decode(&[
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"one","arguments":"{\"x\":"}},
                {"index":1,"id":"b","function":{"name":"two","arguments":"{\"y\":"}}
            ]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"2}"}},
                {"index":0,"function":{"arguments":"1}"}}
            ]}}]}"#,
        ]);

        let partial = buffer.partial();
        let calls: Vec<_> = partial
            .content()
            .filter_map(|c| match c {
                ContentRef::ToolCall(call) => {
                    Some((call.name().to_owned(), call.arguments_raw().to_owned()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("one".to_owned(), r#"{"x":1}"#.to_owned()),
                ("two".to_owned(), r#"{"y":2}"#.to_owned()),
            ]
        );
    }

    #[test]
    fn usage_splits_cached_reads_out_of_the_input_bucket() {
        let (buffer, _) = decode(&[
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":40,
                "total_tokens":1040,"prompt_cache_hit_tokens":600,
                "completion_tokens_details":{"reasoning_tokens":25}}}"#,
        ]);
        let usage = buffer.meta().usage;
        assert_eq!(usage.input, 400, "prompt_tokens includes cached reads");
        assert_eq!(usage.cache_read, 600);
        assert_eq!(usage.output, 40);
        assert_eq!(usage.reasoning, Some(25));
        assert_eq!(usage.total_tokens, 1040);
        assert!(usage.cost.total > 0.0, "cost is priced from the model");
    }

    #[test]
    fn a_reported_model_that_differs_is_recorded() {
        let (buffer, _) = decode(&[r#"{"model":"deepseek-v4-pro","choices":[]}"#]);
        assert_eq!(
            buffer.meta().response_model.as_deref(),
            Some("deepseek-v4-pro")
        );
    }

    #[test]
    fn a_malformed_chunk_is_an_error_not_a_panic() {
        let model = deepseek::flash();
        let meta =
            crate::AssistantMeta::new(model.api.clone(), model.provider.clone(), model.id.clone());
        let mut buffer = MessageBuffer::new(meta);
        let mut decoder = ChunkDecoder::default();
        let mut events = VecDeque::new();
        let result = decoder.apply("{not json", &mut buffer, &model, &mut events);
        assert!(matches!(result, Err(Error::Json(_))));
    }
}
