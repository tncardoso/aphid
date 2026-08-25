//! The on-disk shape of a session, and how it maps to a [`Transcript`].
//!
//! Mapping goes through `aphid-core`'s **public** API in both directions: write
//! by iterating [`MessageRef`], read by replaying the same builders a provider
//! uses. The arena layout stays private, so changing how blocks are packed does
//! not invalidate anyone's saved sessions.
//!
//! One JSON object per line. That means an append is a write with no rewrite, a
//! crash costs at most the turn in flight, and a session is greppable.

use aphid_core::{
    Api, AssistantMeta, ContentInput, ContentRef, Diagnostic, Json, MessageBuffer, MessageRef,
    ProviderId, Role, StopReason, Timestamp, ToolResultMeta, Transcript, Usage,
};
use serde::{Deserialize, Serialize};

/// One line of a session file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    /// The first line, describing the session.
    ///
    /// Boxed only to keep the variants a similar size; there is exactly one
    /// header per file, so the indirection costs nothing that matters.
    Session(Box<Header>),
    /// Everything after it.
    Message(Box<Record>),
}

/// What a session is about.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub id: String,
    /// Where the agent was working, so `--resume` can find the right session.
    pub cwd: String,
    pub started: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub role: Role,
    pub ts: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<AssistantRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultRecord>,
    pub content: Vec<Block>,
}

/// Everything an assistant turn records beyond its content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantRecord {
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

/// Everything a tool result records beyond its content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
}

/// One content block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
    Image {
        mime: String,
        /// Base64. Images are rare enough that the 4/3 expansion is cheaper than
        /// a sidecar file, and it keeps a session a single self-contained file.
        data: String,
    },
}

/// Turn one message into its record.
#[must_use]
pub fn record(message: &MessageRef<'_>) -> Record {
    Record {
        role: message.role(),
        ts: message.timestamp(),
        assistant: message.assistant().map(|meta| AssistantRecord {
            api: meta.api.to_string(),
            provider: meta.provider.to_string(),
            model: meta.model.to_string(),
            response_model: meta.response_model.as_ref().map(ToString::to_string),
            response_id: meta.response_id.as_ref().map(ToString::to_string),
            usage: meta.usage,
            stop: meta.stop_reason,
            raw_stop: meta.raw_stop_reason.as_ref().map(ToString::to_string),
            end_turn: meta.end_turn,
            error: meta.error_message.clone(),
            diagnostics: meta.diagnostics.clone(),
        }),
        tool_result: message.tool_result().map(|meta| ToolResultRecord {
            tool_call_id: meta.tool_call_id.to_string(),
            tool_name: meta.tool_name.to_string(),
            is_error: meta.is_error,
            usage: meta.usage,
            details: meta.details.clone(),
            added_tool_names: meta
                .added_tool_names
                .iter()
                .map(ToString::to_string)
                .collect(),
        }),
        content: message.content().map(block).collect(),
    }
}

fn block(content: ContentRef<'_>) -> Block {
    match content {
        ContentRef::Text(text) => Block::Text {
            text: text.text().to_owned(),
            signature: text.signature().map(ToOwned::to_owned),
        },
        ContentRef::Thinking(thinking) => Block::Thinking {
            text: thinking.text().to_owned(),
            signature: thinking.signature().map(ToOwned::to_owned),
            redacted: thinking.redacted(),
        },
        ContentRef::ToolCall(call) => Block::ToolCall {
            id: call.id().to_owned(),
            name: call.name().to_owned(),
            arguments: call.arguments_raw().to_owned(),
            thought_signature: call.thought_signature().map(ToOwned::to_owned),
            namespace: call.namespace().map(ToOwned::to_owned),
        },
        ContentRef::Image(image) => Block::Image {
            mime: image.mime().to_owned(),
            data: crate::base64::encode(image.data()),
        },
    }
}

/// Append a record to a transcript.
///
/// Assistant turns go through a [`MessageBuffer`], the same path a live stream
/// takes, so their timestamp and metadata are restored exactly. System, user and
/// tool-result messages are appended with the builders the core exposes, which
/// stamp the current time — their original timestamp survives in the file for
/// the log, but not in the rebuilt transcript.
pub fn replay(transcript: &mut Transcript, record: &Record) {
    match record.role {
        Role::Assistant => replay_assistant(transcript, record),
        Role::ToolResult => {
            let meta = match &record.tool_result {
                Some(meta) => {
                    let mut built = ToolResultMeta::new(&meta.tool_call_id, &meta.tool_name);
                    built.is_error = meta.is_error;
                    built.usage = meta.usage;
                    built.details.clone_from(&meta.details);
                    built.added_tool_names = meta.added_tool_names.iter().map(Into::into).collect();
                    built
                }
                // A malformed line should not lose the content it carried.
                None => ToolResultMeta::new("", ""),
            };
            let owned = owned_inputs(&record.content);
            transcript.push_tool_result(meta, &inputs(&owned));
        }
        Role::System | Role::User => {
            let owned = owned_inputs(&record.content);
            let parts = inputs(&owned);
            if record.role == Role::System {
                // The core has no `push_system_parts`; a system prompt is text.
                let text: String = owned
                    .iter()
                    .filter_map(|part| match part {
                        Owned::Text(text) => Some(text.as_str()),
                        Owned::Image { .. } => None,
                    })
                    .collect();
                transcript.push_system(&text);
            } else {
                transcript.push_user_parts(&parts);
            }
        }
    }
}

fn replay_assistant(transcript: &mut Transcript, record: &Record) {
    let stored = record.assistant.as_ref();
    let mut meta = AssistantMeta::new(
        stored.map_or(Api::OpenAiCompletions, |meta| {
            meta.api.parse().unwrap_or(Api::OpenAiCompletions)
        }),
        stored.map_or_else(
            || ProviderId::new(""),
            |meta| ProviderId::new(meta.provider.as_str()),
        ),
        stored.map_or("", |meta| meta.model.as_str()),
    );
    if let Some(stored) = stored {
        meta.response_model = stored.response_model.as_deref().map(Into::into);
        meta.response_id = stored.response_id.as_deref().map(Into::into);
        meta.usage = stored.usage;
        meta.stop_reason = stored.stop;
        meta.raw_stop_reason = stored.raw_stop.as_deref().map(Into::into);
        meta.end_turn = stored.end_turn;
        meta.error_message.clone_from(&stored.error);
        meta.diagnostics.clone_from(&stored.diagnostics);
    }

    let mut buffer = MessageBuffer::new(meta);
    for block in &record.content {
        match block {
            Block::Text { text, signature } => {
                let index = buffer.begin_text();
                buffer.push_delta(index, text);
                if let Some(signature) = signature {
                    buffer.set_signature(index, signature);
                }
            }
            Block::Thinking {
                text,
                signature,
                redacted,
            } => {
                let index = buffer.begin_thinking();
                buffer.push_delta(index, text);
                if let Some(signature) = signature {
                    buffer.set_signature(index, signature);
                }
                if *redacted {
                    buffer.set_redacted(index, true);
                }
            }
            Block::ToolCall {
                id,
                name,
                arguments,
                thought_signature,
                namespace,
            } => {
                let index = buffer.begin_tool_call(id.as_str(), name.as_str());
                buffer.push_delta(index, arguments);
                if let Some(signature) = thought_signature {
                    buffer.set_thought_signature(index, signature);
                }
                if let Some(namespace) = namespace {
                    buffer.set_namespace(index, namespace.as_str());
                }
            }
            Block::Image { mime, data } => {
                let bytes = crate::base64::decode(data).unwrap_or_default();
                buffer.push_image(&bytes, mime);
            }
        }
    }
    buffer.set_timestamp(record.ts);
    transcript.commit(buffer);
}

/// Owned content, so the borrowed `ContentInput` slice has something to point at.
enum Owned {
    Text(String),
    Image { data: Vec<u8>, mime: String },
}

fn owned_inputs(blocks: &[Block]) -> Vec<Owned> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text, .. } | Block::Thinking { text, .. } => {
                Some(Owned::Text(text.clone()))
            }
            Block::Image { mime, data } => Some(Owned::Image {
                data: crate::base64::decode(data).unwrap_or_default(),
                mime: mime.clone(),
            }),
            // A user or tool-result message cannot hold a tool call.
            Block::ToolCall { .. } => None,
        })
        .collect()
}

fn inputs(owned: &[Owned]) -> Vec<ContentInput<'_>> {
    owned
        .iter()
        .map(|part| match part {
            Owned::Text(text) => ContentInput::Text(text),
            Owned::Image { data, mime } => ContentInput::Image { data, mime },
        })
        .collect()
}
