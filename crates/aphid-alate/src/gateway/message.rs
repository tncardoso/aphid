//! The per-session tool that lets a scheduled job speak in the conversation
//! that scheduled it.
//!
//! A job runs in a session of its own, which nobody is watching: what it says
//! goes to its transcript and to `alate.log`, and to no person. This is how it
//! reaches one — deliberately, one message at a time, and only into the
//! conversation recorded on the job.
//!
//! **No permission gate**, unlike its sibling [`super::attachment`]. The
//! destination is not the model's to choose: it is the conversation the person
//! created the job in, so the consent is in the scheduling. And under
//! `permissions: ask` the question would go to that same conversation, wait out
//! its five minutes unanswered at three in the morning, and refuse the send —
//! which is exactly the case this tool exists for. That is the reversible part
//! of the design; a gate belongs here the day a job can name its own
//! destination.

use std::sync::Arc;

use aphid_agent::Toolbox;
use aphid_agent::rt::{Component, Composition, Context};
use aphid_agent::{ToolCx, ToolOutcome, tool_fn};
use serde::Deserialize;

use super::Publisher;
use super::origin::Origin;

pub const NAME: &str = "send_message";

/// Installs the message tool for one job's session only.
pub struct MessageComponent {
    /// The session this tool is offered to — a job's, never a person's.
    session: String,
    /// What the message says it is from, as `/sessions` would name it.
    from: String,
    origin: Origin,
    publisher: Publisher,
    tools: Arc<Toolbox>,
}

impl MessageComponent {
    #[must_use]
    pub fn new(
        session: String,
        from: String,
        origin: Origin,
        publisher: Publisher,
        composition: &Composition,
    ) -> Self {
        Self {
            session,
            from,
            origin,
            publisher,
            tools: Arc::clone(&composition.tools),
        }
    }
}

impl Component for MessageComponent {
    fn name(&self) -> &str {
        "gateway-message"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        self.tools.contribute_scoped(
            ctx,
            self.session.clone(),
            Arc::new(tool(
                self.from.clone(),
                self.origin.clone(),
                self.publisher.clone(),
            )),
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct Params {
    text: String,
}

/// Build the tool that speaks into one conversation.
#[must_use]
pub fn tool(from: String, origin: Origin, publisher: Publisher) -> impl aphid_agent::ToolHandler {
    let description = format!(
        "Say something in {}, the conversation that scheduled this job. This session is not \
         being watched by anybody, so this is the only way what you found reaches a person. Send \
         one message, when you have something worth saying; a job that found nothing to report \
         should say nothing.",
        origin.label
    );
    tool_fn(
        NAME,
        description,
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "What to say. Write it for somebody who has not seen this session." }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
        move |params: Params, cx: ToolCx| {
            let from = from.clone();
            let origin = origin.clone();
            let publisher = publisher.clone();
            async move {
                if cx.cancelled() {
                    return ToolOutcome::error("the message was cancelled before it was sent");
                }
                let text = params.text.trim();
                if text.is_empty() {
                    return ToolOutcome::error("a message needs something to say");
                }
                match publisher.deliver(&origin, &from, text) {
                    Ok(_) => ToolOutcome::text(format!("said in {}", origin.label)),
                    Err(error) => ToolOutcome::error(error),
                }
            }
        },
    )
    .sequential()
}
