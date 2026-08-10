//! Registries for tools and plugins.
//!
//! Both are laid out for the read path. Tool declarations live in one contiguous
//! `Vec<Tool>` because that is exactly what a request encoder wants, and the
//! handlers sit in a second vector at the same indices. Plugins are indexed by
//! hook, so dispatch walks a list of subscribers rather than every plugin.

use std::sync::Arc;

use aphid_core::{Event, MessageId, Tool};

use crate::RunOutcome;
use crate::plugin::{
    Flow, Guard, Interest, PendingCall, Plugin, PromptDraft, ResultCx, RunCx, StreamCx, TurnCx,
    TurnSummary,
};
use crate::tool::{ToolHandler, ToolOutcome};

const HOOK_RUN_START: usize = 0;
const HOOK_TURN_START: usize = 1;
const HOOK_EVENT: usize = 2;
const HOOK_TOOL_CALL: usize = 3;
const HOOK_TOOL_RESULT: usize = 4;
const HOOK_TURN_END: usize = 5;
const HOOK_RUN_END: usize = 6;
const HOOK_TOOL_PROGRESS: usize = 7;
const HOOK_PROMPT: usize = 8;
const HOOK_MESSAGE: usize = 9;

/// Registered tools, split into the half the provider sees and the half that
/// runs.
#[derive(Clone, Default)]
pub struct Tools {
    declarations: Vec<Tool>,
    handlers: Vec<Arc<dyn ToolHandler>>,
}

impl Tools {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. A later registration under the same name replaces the
    /// earlier one, which is how a plugin overrides a built-in.
    pub fn push(&mut self, handler: Arc<dyn ToolHandler>) {
        let declaration = handler.declaration().clone();
        match self.index_of(&declaration.name) {
            Some(index) => {
                self.declarations[index] = declaration;
                self.handlers[index] = handler;
            }
            None => {
                self.declarations.push(declaration);
                self.handlers.push(handler);
            }
        }
    }

    /// The declarations, contiguous and ready to hand to a request encoder.
    #[must_use]
    pub fn declarations(&self) -> &[Tool] {
        &self.declarations
    }

    /// Tool counts are in the tens, so a scan beats a map and its allocation.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.declarations
            .iter()
            .position(|tool| tool.name.as_str() == name)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolHandler>> {
        self.index_of(name).map(|index| &self.handlers[index])
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.declarations.iter().map(|tool| tool.name.as_str())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// Registered plugins plus the per-hook dispatch table.
#[derive(Clone, Default)]
pub struct Plugins {
    plugins: Vec<Arc<dyn Plugin>>,
    /// One list of plugin indices per hook, built at registration.
    by_hook: [Vec<u16>; Interest::COUNT],
}

impl Plugins {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin and file it under each hook it declared an interest in.
    ///
    /// # Panics
    ///
    /// Panics past 65_536 plugins, which would mean something has gone very
    /// wrong with plugin discovery.
    pub fn push(&mut self, plugin: Arc<dyn Plugin>) {
        let index = u16::try_from(self.plugins.len()).expect("at most 65_536 plugins");
        let interests = plugin.interests();
        self.plugins.push(plugin);
        for (hook, subscribers) in self.by_hook.iter_mut().enumerate() {
            if interests.contains(Interest::bit(hook)) {
                subscribers.push(index);
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|plugin| plugin.name())
    }

    /// Every tool contributed by a plugin, in registration order.
    pub(crate) fn tools(&self) -> Vec<Arc<dyn ToolHandler>> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.tools())
            .collect()
    }

    /// Whether anybody is listening to the per-token hook. Checked once per
    /// stream so an unobserved run skips the dispatch entirely.
    pub(crate) fn observes_events(&self) -> bool {
        !self.by_hook[HOOK_EVENT].is_empty()
    }

    /// Show a prompt to every subscriber before it is appended. All of them run
    /// even once one has rejected it — an observer still wants to see a prompt
    /// somebody else turned away — but the first rejection is the one reported.
    pub(crate) fn prompt(&self, draft: &mut PromptDraft<'_>) {
        for &index in &self.by_hook[HOOK_PROMPT] {
            self.plugins[index as usize].on_prompt(draft);
        }
    }

    pub(crate) fn message(&self, cx: &mut TurnCx<'_>, message: MessageId) {
        for &index in &self.by_hook[HOOK_MESSAGE] {
            self.plugins[index as usize].on_message(cx, message);
        }
    }

    /// Whether anybody wants to see prompts. Checked before building a draft, so
    /// an unobserved prompt never copies its text.
    pub(crate) fn observes_prompts(&self) -> bool {
        !self.by_hook[HOOK_PROMPT].is_empty()
    }

    pub(crate) fn run_start(&self, cx: &mut RunCx<'_>) {
        for &index in &self.by_hook[HOOK_RUN_START] {
            self.plugins[index as usize].on_run_start(cx);
        }
    }

    pub(crate) fn turn_start(&self, cx: &mut TurnCx<'_>) {
        for &index in &self.by_hook[HOOK_TURN_START] {
            self.plugins[index as usize].on_turn_start(cx);
        }
    }

    pub(crate) fn event(&self, event: &Event, cx: &StreamCx<'_>) {
        for &index in &self.by_hook[HOOK_EVENT] {
            self.plugins[index as usize].on_event(event, cx);
        }
    }

    /// Preflight one call. Every subscriber runs — an observer still wants to
    /// see a call somebody else blocked — but the first block is the one that
    /// takes effect.
    pub(crate) fn tool_call(&self, call: &mut PendingCall<'_>) {
        for &index in &self.by_hook[HOOK_TOOL_CALL] {
            match self.plugins[index as usize].on_tool_call(call) {
                Guard::Allow => {}
                blocked @ Guard::Block { .. } => {
                    if call.block.is_none() {
                        call.block = Some(blocked);
                    }
                }
            }
        }
    }

    pub(crate) fn tool_progress(&self, call_id: &str, tool: &str, chunk: &str) {
        for &index in &self.by_hook[HOOK_TOOL_PROGRESS] {
            self.plugins[index as usize].on_tool_progress(call_id, tool, chunk);
        }
    }

    /// Whether anybody is listening for partial tool output. A tool can check
    /// this through [`ToolCx`](crate::ToolCx) before doing work only a UI needs.
    pub(crate) fn observes_progress(&self) -> bool {
        !self.by_hook[HOOK_TOOL_PROGRESS].is_empty()
    }

    pub(crate) fn tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        for &index in &self.by_hook[HOOK_TOOL_RESULT] {
            self.plugins[index as usize].on_tool_result(outcome, cx);
        }
    }

    pub(crate) fn turn_end(&self, cx: &mut TurnCx<'_>, turn: &TurnSummary) -> Flow {
        let mut flow = Flow::Continue;
        for &index in &self.by_hook[HOOK_TURN_END] {
            if self.plugins[index as usize].on_turn_end(cx, turn) == Flow::Stop {
                flow = Flow::Stop;
            }
        }
        flow
    }

    pub(crate) fn run_end(&self, cx: &mut RunCx<'_>, outcome: &RunOutcome) {
        for &index in &self.by_hook[HOOK_RUN_END] {
            self.plugins[index as usize].on_run_end(cx, outcome);
        }
    }
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}
