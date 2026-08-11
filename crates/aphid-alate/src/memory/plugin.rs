//! How the agent reaches its memory: two tools, and one recall it never asks
//! for.
//!
//! The automatic recall is the reason an alate feels resident rather than new
//! each time. It runs on the prompt itself, before the model sees anything, and
//! the facts arrive as a system note — not folded into what the person said, so
//! the model can always tell the two apart.

use std::sync::{Arc, Mutex};

use aphid_agent::{Cx, Interest, Plugin, PromptDraft, ToolHandler, ToolOutcome, tool_fn};
use aphid_core::Json;
use serde::Deserialize;

use super::{Hit, Shared, lock, normalise};
use crate::config::MemoryConfig;

/// How many facts `recall` gives back when the model does not say.
const RECALL_LIMIT: usize = 10;

/// The most a single `recall` may ask for. A tool that can pour the whole
/// memory into the context is one the model will eventually use that way.
const RECALL_CEILING: usize = 100;

#[derive(Debug, Deserialize)]
pub struct RememberParams {
    pub path: String,
    pub fact: String,
}

#[derive(Debug, Deserialize)]
pub struct RecallParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `remember` — write one fact.
#[must_use]
pub fn remember_tool(memory: Shared) -> impl ToolHandler {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Where the fact belongs, such as /projects/aphid or /people/thiago"
            },
            "fact": {
                "type": "string",
                "description": "One short sentence that will still be true tomorrow"
            }
        },
        "required": ["path", "fact"],
        "additionalProperties": false
    });
    let description = "Write one fact to your memory, so you still know it in a later session. \
                       Keep a fact short and self-contained: one sentence, and no pronoun that \
                       points outside it. File it under a path that names the topic; a path is \
                       made the first time you use it. Prefer adding a fact to rewriting one."
        .to_owned();

    tool_fn(
        "remember",
        description,
        schema,
        move |params: RememberParams, _cx| {
            let memory = memory.clone();
            async move {
                let done = tokio::task::spawn_blocking(move || {
                    lock(&memory)
                        .store(&params.path, &params.fact)
                        .and_then(|()| normalise(&params.path))
                })
                .await;

                match done {
                    Ok(Ok(path)) => ToolOutcome::text(format!("remembered, under {path}")),
                    Ok(Err(error)) => ToolOutcome::error(error.to_string()),
                    Err(error) => ToolOutcome::error(format!("the memory did not answer: {error}")),
                }
            }
        },
    )
}

/// `recall` — read what the memory holds.
#[must_use]
pub fn recall_tool(memory: Shared) -> impl ToolHandler {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "What you want to know. Leave it out for the newest facts."
            },
            "path": {
                "type": "string",
                "description": "Search only at or below this path"
            },
            "limit": {
                "type": "number",
                "description": "How many facts to give back"
            }
        },
        "additionalProperties": false
    });
    let description = "Search your memory. Use it before you say you do not know something, and \
                       before you ask for something you may already have been told. With no \
                       query it gives the newest facts."
        .to_owned();

    tool_fn(
        "recall",
        description,
        schema,
        move |params: RecallParams, _cx| {
            let memory = memory.clone();
            async move {
                let limit = params
                    .limit
                    .unwrap_or(RECALL_LIMIT)
                    .clamp(1, RECALL_CEILING);
                let done = tokio::task::spawn_blocking(move || {
                    lock(&memory).recall(&params.query, params.path.as_deref(), limit)
                })
                .await;

                match done {
                    Ok(Ok(hits)) if hits.is_empty() => {
                        ToolOutcome::text("the memory holds nothing about that")
                    }
                    Ok(Ok(hits)) => {
                        let mut outcome = ToolOutcome::text(render(&hits));
                        outcome.details = Some(details(&hits));
                        outcome
                    }
                    Ok(Err(error)) => ToolOutcome::error(error.to_string()),
                    Err(error) => ToolOutcome::error(format!("the memory did not answer: {error}")),
                }
            }
        },
    )
}

/// The facts, one to a line.
fn render(hits: &[Hit]) -> String {
    hits.iter()
        .map(|hit| format!("{} · {:.2} · {}\n", hit.path, hit.score, hit.fact))
        .collect()
}

fn details(hits: &[Hit]) -> Json {
    serde_json::json!({
        "facts": hits
            .iter()
            .map(|hit| serde_json::json!({
                "path": hit.path,
                "fact": hit.fact,
                "score": hit.score,
            }))
            .collect::<Vec<_>>()
    })
}

/// Offers the memory unasked, and ships the two tools.
///
/// The recall happens at [`Plugin::on_prompt`], which is the only hook that can
/// read the prompt, and the facts are appended at [`Plugin::on_run_start`],
/// which is the first that has a transcript to append to. A run that appended
/// no prompt — a resume — finds nothing waiting and adds nothing.
pub struct MemoryPlugin {
    memory: Shared,
    limit: usize,
    /// What the last prompt recalled, waiting for a transcript to go into.
    pending: Mutex<Option<String>>,
}

impl MemoryPlugin {
    #[must_use]
    pub fn new(memory: Shared, config: &MemoryConfig) -> Self {
        Self {
            memory,
            limit: config.recall,
            pending: Mutex::new(None),
        }
    }
}

impl Plugin for MemoryPlugin {
    fn name(&self) -> &str {
        "memory"
    }

    fn interests(&self) -> Interest {
        Interest::PROMPT | Interest::RUN_START
    }

    fn tools(&self) -> Vec<Arc<dyn ToolHandler>> {
        vec![
            Arc::new(remember_tool(self.memory.clone())),
            Arc::new(recall_tool(self.memory.clone())),
        ]
    }

    fn on_prompt(&self, draft: &mut PromptDraft<'_>) {
        if self.limit == 0 {
            return;
        }
        // A memory that cannot be read must not stop a conversation. The agent
        // still has `recall`, which reports the failure where it can be acted
        // on, rather than here where nobody asked anything.
        let Ok(hits) = lock(&self.memory).recall(draft.text(), None, self.limit) else {
            return;
        };
        if hits.is_empty() {
            return;
        }

        let mut note = String::from(
            "<recalled_facts>\nFrom your memory, and possibly relevant to what was just said. \
             Trust what you are told now over any of these.\n",
        );
        for hit in &hits {
            note.push_str(&format!("{} · {}\n", hit.path, hit.fact));
        }
        note.push_str("</recalled_facts>");

        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(note);
        }
    }

    fn on_run_start(&self, cx: &mut Cx<'_>) {
        let note = match self.pending.lock() {
            Ok(mut pending) => pending.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(note) = note {
            cx.push_system_note(&note);
        }
    }
}
