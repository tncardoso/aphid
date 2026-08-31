//! How the agent reaches its memory: two tools, and one recall it never asks
//! for.
//!
//! The automatic recall is the reason an alate feels resident rather than new
//! each time. It runs on the prompt itself, before the model sees anything, and
//! the facts arrive as a system note — not folded into what the person said, so
//! the model can always tell the two apart.

use std::sync::{Arc, Mutex};

use aphid_agent::rt::{Bus, Component, Composition, Context, Disposer, Scope};
use aphid_agent::{Prompt, RunStart, ToolHandler, ToolOutcome, Toolbox, tool_fn};
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
/// The recall happens on [`Prompt`], the only announcement that can read what
/// was typed, and the facts are appended on [`RunStart`], the first that has a
/// transcript to append to. A run that appended no prompt — a resume — finds
/// nothing waiting and adds nothing.
pub struct MemoryComponent {
    memory: Shared,
    limit: usize,
    /// What the last prompt recalled, waiting for a transcript to go into.
    pending: Arc<Mutex<Option<String>>>,
    bus: Arc<Bus>,
    tools: Arc<Toolbox>,
    /// The conversation this component recalls for, or `None` for a standalone
    /// agent. The `pending` slot belongs to one session: a prompt recalled by
    /// another session must not feed this one's next run.
    scope: Scope,
}

impl MemoryComponent {
    #[must_use]
    pub fn new(
        scope: Scope,
        memory: Shared,
        config: &MemoryConfig,
        composition: &Composition,
    ) -> Self {
        Self {
            memory,
            limit: config.recall,
            pending: Arc::default(),
            bus: Arc::clone(&composition.bus),
            tools: Arc::clone(&composition.tools),
            scope,
        }
    }
}

/// What the memory has to say about a prompt, if anything.
///
/// A free function rather than a method: the listener that calls it outlives
/// the borrow of the component that registered it, and needs nothing from it
/// beyond these two values.
fn recalled(memory: &Shared, limit: usize, text: &str) -> Option<String> {
    {
        if limit == 0 {
            return None;
        }
        // A memory that cannot be read must not stop a conversation. The agent
        // still has `recall`, which reports the failure where it can be acted
        // on, rather than here where nobody asked anything.
        let hits = lock(memory).recall(text, None, limit).ok()?;
        if hits.is_empty() {
            return None;
        }

        let mut note = String::from(
            "<recalled_facts>\nFrom your memory, and possibly relevant to what was just said. \
             Trust what you are told now over any of these.\n",
        );
        for hit in &hits {
            note.push_str(&format!("{} · {}\n", hit.path, hit.fact));
        }
        note.push_str("</recalled_facts>");
        Some(note)
    }
}

impl Component for MemoryComponent {
    fn name(&self) -> &str {
        "memory"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        let owner = ctx.uid();

        self.tools
            .contribute(ctx, Arc::new(remember_tool(self.memory.clone())));
        self.tools
            .contribute(ctx, Arc::new(recall_tool(self.memory.clone())));

        let memory = self.memory.clone();
        let limit = self.limit;
        let pending = Arc::clone(&self.pending);
        let scope = self.scope.clone();
        self.bus.on_scoped::<Prompt>(scope, owner, move |prompt| {
            let note = recalled(&memory, limit, &prompt.text);
            if let Ok(mut slot) = pending.lock() {
                *slot = note;
            }
        });

        let pending = Arc::clone(&self.pending);
        let scope = self.scope.clone();
        self.bus.on_scoped::<RunStart>(scope, owner, move |start| {
            let note = match pending.lock() {
                Ok(mut slot) => slot.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(note) = note {
                start.0.note(note);
            }
        });

        let bus = Arc::clone(&self.bus);
        ctx.effect(move || {
            Disposer::sync(move || {
                bus.unsubscribe::<Prompt>(owner);
                bus.unsubscribe::<RunStart>(owner);
            })
        });
        Ok(())
    }
}
