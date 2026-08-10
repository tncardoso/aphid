//! The bridge between the agent's hooks and the loaded scripts.
//!
//! [`PluginHost`] is one [`Plugin`] standing in for every `.rhai` file, so the
//! agent's registry sees a single subscriber however many scripts are loaded.
//! Its [`Plugin::interests`] is the union of theirs, and each hook visits only
//! the scripts that define it.
//!
//! # What a hook may change
//!
//! Rhai passes arguments by value, so a script cannot mutate a payload in place.
//! Every payload arrives as a map and every change comes back as the return
//! value: unit changes nothing, a verdict from `block(…)` or `stop()` steers the
//! run, and a map patches named fields. The exception is `cx`, which carries an
//! `Arc` and so records what it is told regardless of cloning.
//!
//! # When a script fails
//!
//! Failures are **open**: the error is reported and the hook is skipped, because
//! a broken plugin should not take a session with it. The one exception is
//! `on_tool_call`, which is **closed** — a guard that raised has not decided
//! anything, and running the tool anyway would defeat the only hook people write
//! for safety.

use std::sync::Arc;

use aphid_agent::{
    Cx, Flow, Guard, Interest, PendingCall, Plugin, PromptDraft, ResultCx, RunOutcome, StreamCx,
    ToolContent, ToolOutcome, TurnSummary,
};
use aphid_core::{BlockKind, ContentRef, Event, MessageId, StopReason};
use rhai::{Dynamic, Map};

use crate::caps::{Capabilities, Silent, Sink};
use crate::convert;
use crate::cx::ScriptCx;
use crate::discover::{Diagnostic, PluginFile};
use crate::script::ScriptPlugin;
use crate::worker::Worker;

/// Every loaded plugin, as one agent plugin.
pub struct PluginHost {
    plugins: Vec<Arc<ScriptPlugin>>,
    diagnostics: Vec<Diagnostic>,
    interests: Interest,
    /// Set while a notice is being dispatched, so a hook that notifies cannot
    /// call itself back.
    in_notice: std::sync::atomic::AtomicBool,
    /// Set while a tick is being dispatched, so a slow one is skipped rather
    /// than queued behind itself.
    in_tick: std::sync::atomic::AtomicBool,
}

impl PluginHost {
    /// Load every file, keeping the ones that compile.
    ///
    /// A file that fails becomes a [`Diagnostic`] and the rest still load, the
    /// same bargain the model catalog and skills make: a broken configuration
    /// file is worth saying out loud, not worth refusing to start over.
    #[must_use]
    pub fn load(
        files: &[PluginFile],
        caps: &Capabilities,
        sink: Arc<dyn Sink>,
    ) -> (Self, Vec<Diagnostic>) {
        let worker = Arc::new(Worker::spawn());
        let mut plugins = Vec::new();
        let mut diagnostics = Vec::new();

        for file in files {
            match ScriptPlugin::load(file, caps, &sink, &worker) {
                Ok(plugin) => plugins.push(Arc::new(plugin)),
                Err(message) => diagnostics.push(Diagnostic {
                    path: file.path.clone(),
                    message,
                }),
            }
        }

        let interests = plugins
            .iter()
            .fold(Interest::empty(), |set, plugin| set | plugin.interests());

        let host = Self {
            plugins,
            diagnostics: diagnostics.clone(),
            interests,
            in_notice: std::sync::atomic::AtomicBool::new(false),
            in_tick: std::sync::atomic::AtomicBool::new(false),
        };
        (host, diagnostics)
    }

    /// A host with nothing loaded, for `--no-plugins` and for tests.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            diagnostics: Vec::new(),
            interests: Interest::empty(),
            in_notice: std::sync::atomic::AtomicBool::new(false),
            in_tick: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    #[must_use]
    pub fn plugins(&self) -> &[Arc<ScriptPlugin>] {
        &self.plugins
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Whether any script defines a hook.
    ///
    /// Callers use this to skip building a decorator nobody would use — the
    /// permission chain, the request backend — so the common case of a plugin
    /// that watches one thing pays for that one thing only.
    #[must_use]
    pub fn any_defines(&self, hook: &str) -> bool {
        self.plugins.iter().any(|plugin| plugin.defines(hook))
    }

    /// The scripts that define a hook.
    pub(crate) fn defining<'a>(
        &'a self,
        hook: &'a str,
    ) -> impl Iterator<Item = &'a Arc<ScriptPlugin>> {
        self.plugins.iter().filter(move |p| p.defines(hook))
    }

    /// Claim the notice path. `true` means somebody else already has it.
    pub(crate) fn enter_notice(&self) -> bool {
        self.in_notice
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn leave_notice(&self) {
        self.in_notice
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Claim the tick path. `true` means the last tick has not finished.
    pub(crate) fn enter_tick(&self) -> bool {
        self.in_tick
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn leave_tick(&self) {
        self.in_tick
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Call a context-taking hook and apply whatever it recorded.
    fn with_cx(&self, hook: &str, cx: &mut Cx<'_>, extra: Option<Dynamic>) {
        for plugin in self.defining(hook) {
            let script_cx = ScriptCx::new(cx);
            let returned = match extra.clone() {
                Some(extra) => plugin.call(hook, (script_cx.clone(), extra)),
                None => plugin.call(hook, (script_cx.clone(),)),
            };
            script_cx.apply(cx);
            drop(returned);
        }
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.plugins.iter().map(|p| p.name()))
            .finish()
    }
}

impl Plugin for PluginHost {
    fn name(&self) -> &str {
        "rhai"
    }

    fn interests(&self) -> Interest {
        self.interests
    }

    fn tools(&self) -> Vec<Arc<dyn aphid_agent::ToolHandler>> {
        self.plugins.iter().flat_map(ScriptPlugin::tools).collect()
    }

    fn on_prompt(&self, draft: &mut PromptDraft<'_>) {
        for plugin in self.defining("on_prompt") {
            if draft.is_rejected() {
                // Later hooks still see the prompt, but nothing they return can
                // un-reject it, so there is nothing left to apply.
                break;
            }

            let mut payload = Map::new();
            payload.insert("text".into(), draft.text().into());

            let Some(returned) = plugin.call("on_prompt", (payload,)) else {
                continue;
            };

            match verdict(&returned) {
                Some(("reject", reason)) => draft.reject(reason),
                _ => {
                    if let Some(text) = field_string(&returned, "text") {
                        draft.set_text(text);
                    } else if returned.is_string() {
                        draft.set_text(returned.into_string().unwrap_or_default());
                    }
                }
            }
        }
    }

    fn on_run_start(&self, cx: &mut Cx<'_>) {
        self.with_cx("on_run_start", cx, None);
    }

    fn on_turn_start(&self, cx: &mut Cx<'_>) {
        self.with_cx("on_turn_start", cx, None);
    }

    fn on_event(&self, event: &Event, cx: &StreamCx<'_>) {
        // The hot path. The payload is built once for all subscribers, and only
        // when somebody actually subscribed.
        let payload = event_map(event, cx);
        for plugin in self.defining("on_event") {
            plugin.call("on_event", (payload.clone(),));
        }
    }

    fn on_message(&self, cx: &mut Cx<'_>, message: MessageId) {
        let payload = Dynamic::from_map(message_map(cx, message));
        self.with_cx("on_message", cx, Some(payload));
    }

    fn on_tool_call(&self, call: &mut PendingCall<'_>) -> Guard {
        let mut guard = Guard::Allow;

        for plugin in self.defining("on_tool_call") {
            let mut payload = Map::new();
            payload.insert("id".into(), call.id().into());
            payload.insert("name".into(), call.name().into());
            payload.insert("arguments".into(), call.arguments().into());
            payload.insert("known".into(), call.is_known().into());
            payload.insert("blocked".into(), call.is_blocked().into());

            let Some(returned) = plugin.call("on_tool_call", (payload,)) else {
                // Fail closed: a guard that raised has not allowed anything.
                if !plugin.defines("on_tool_call") {
                    continue;
                }
                if matches!(guard, Guard::Allow) {
                    guard = Guard::block(format!("plugin `{}` failed to decide", plugin.name()));
                }
                continue;
            };

            match verdict(&returned) {
                Some(("block", reason)) => {
                    if matches!(guard, Guard::Allow) {
                        guard = Guard::block(reason);
                    }
                }
                Some(("block_and_stop", reason)) => {
                    if matches!(guard, Guard::Allow) {
                        guard = Guard::block_and_stop(reason);
                    }
                }
                _ => {
                    if let Some(arguments) = field_string(&returned, "arguments") {
                        call.set_arguments(arguments);
                    }
                }
            }
        }

        guard
    }

    fn on_tool_progress(&self, call_id: &str, tool: &str, chunk: &str) {
        for plugin in self.defining("on_tool_progress") {
            plugin.call(
                "on_tool_progress",
                (call_id.to_owned(), tool.to_owned(), chunk.to_owned()),
            );
        }
    }

    fn on_tool_result(&self, outcome: &mut ToolOutcome, cx: &ResultCx<'_>) {
        for plugin in self.defining("on_tool_result") {
            let mut payload = Map::new();
            payload.insert("id".into(), cx.id().into());
            payload.insert("name".into(), cx.name().into());
            payload.insert("arguments".into(), cx.arguments().into());
            payload.insert("turn".into(), i64::from(cx.turn()).into());
            payload.insert("content".into(), outcome.text_content().into());
            payload.insert("is_error".into(), outcome.is_error.into());
            payload.insert(
                "details".into(),
                outcome
                    .details
                    .as_ref()
                    .map_or(Dynamic::UNIT, convert::to_dynamic),
            );

            let Some(returned) = plugin.call("on_tool_result", (payload,)) else {
                continue;
            };
            let Some(patch) = as_map(&returned) else {
                continue;
            };

            if let Some(content) = patch.get("content") {
                outcome.content = vec![ToolContent::Text(content.to_string())];
            }
            if let Some(flag) = patch.get("is_error").and_then(|v| v.as_bool().ok()) {
                outcome.is_error = flag;
            }
            if let Some(details) = patch.get("details") {
                outcome.details = if details.is_unit() {
                    None
                } else {
                    Some(convert::to_json(details))
                };
            }
        }
    }

    fn on_turn_end(&self, cx: &mut Cx<'_>, turn: &TurnSummary) -> Flow {
        let mut flow = Flow::Continue;

        for plugin in self.defining("on_turn_end") {
            let mut payload = Map::new();
            payload.insert("stop_reason".into(), stop_reason(turn.stop_reason).into());
            payload.insert(
                "tool_calls".into(),
                i64::try_from(turn.tool_calls).unwrap_or(i64::MAX).into(),
            );
            payload.insert("input".into(), i64::from(turn.usage.input).into());
            payload.insert("output".into(), i64::from(turn.usage.output).into());
            payload.insert(
                "error".into(),
                turn.error
                    .as_ref()
                    .map_or(Dynamic::UNIT, |text| text.clone().into()),
            );

            let script_cx = ScriptCx::new(cx);
            let returned = plugin.call("on_turn_end", (script_cx.clone(), payload));
            script_cx.apply(cx);

            if let Some(returned) = returned
                && matches!(verdict(&returned), Some(("stop", _)))
            {
                flow = Flow::Stop;
            }
        }

        flow
    }

    fn on_run_end(&self, cx: &mut Cx<'_>, outcome: &RunOutcome) {
        let mut payload = Map::new();
        payload.insert("stop".into(), stop_reason(outcome.stop).into());
        payload.insert("turns".into(), i64::from(outcome.turns).into());
        payload.insert("input".into(), i64::from(outcome.usage.input).into());
        payload.insert("output".into(), i64::from(outcome.usage.output).into());
        payload.insert(
            "error".into(),
            outcome
                .error
                .as_ref()
                .map_or(Dynamic::UNIT, |text| text.clone().into()),
        );

        self.with_cx("on_run_end", cx, Some(Dynamic::from_map(payload)));

        // A run is the natural save point: whatever a plugin learned this run is
        // on disk before the next prompt, and a session that never ends cleanly
        // still keeps everything up to its last turn.
        self.flush();
    }
}

/// A host that loads nothing, for a caller that wants the type without the cost.
#[must_use]
pub fn silent_sink() -> Arc<dyn Sink> {
    Arc::new(Silent)
}

/// Read a `#{ verdict: …, reason: … }` map, as `block(…)` and friends build.
fn verdict(value: &Dynamic) -> Option<(&'static str, String)> {
    let map = as_map(value)?;
    let kind = map.get("verdict")?.clone().into_string().ok()?;
    let reason = map
        .get("reason")
        .map(std::string::ToString::to_string)
        .unwrap_or_default();

    let known = [
        "block",
        "block_and_stop",
        "reject",
        "stop",
        "allow",
        "notice",
    ]
    .into_iter()
    .find(|name| *name == kind)?;

    Some((known, reason))
}

/// A `Dynamic` as a map, when it is one.
pub(crate) fn map_of(value: &Dynamic) -> Option<Map> {
    as_map(value)
}

fn as_map(value: &Dynamic) -> Option<Map> {
    value.is_map().then(|| value.clone().cast::<Map>())
}

fn field_string(value: &Dynamic, key: &str) -> Option<String> {
    let map = as_map(value)?;
    let field = map.get(key)?;
    field.is_string().then(|| field.to_string())
}

fn stop_reason(stop: StopReason) -> &'static str {
    match stop {
        StopReason::Pending => "pending",
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "tool_use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

fn event_map(event: &Event, cx: &StreamCx<'_>) -> Map {
    let mut map = Map::new();
    map.insert("turn".into(), i64::from(cx.turn()).into());

    match *event {
        Event::Start => {
            map.insert("kind".into(), "start".into());
        }
        Event::BlockStart { index, kind } => {
            map.insert("kind".into(), "block_start".into());
            map.insert("index".into(), i64::from(index).into());
            map.insert("block".into(), block_kind(kind).into());
        }
        Event::Delta { index, kind, span } => {
            map.insert("kind".into(), "delta".into());
            map.insert("index".into(), i64::from(index).into());
            map.insert("block".into(), block_kind(kind).into());
            map.insert("text".into(), cx.text(span).into());
        }
        Event::BlockEnd { index } => {
            map.insert("kind".into(), "block_end".into());
            map.insert("index".into(), i64::from(index).into());
        }
        Event::Done { stop } => {
            map.insert("kind".into(), "done".into());
            map.insert("stop".into(), stop_reason(stop).into());
        }
        Event::Error { stop } => {
            map.insert("kind".into(), "error".into());
            map.insert("stop".into(), stop_reason(stop).into());
        }
    }

    map
}

fn block_kind(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Text => "text",
        BlockKind::Thinking => "thinking",
        BlockKind::Image => "image",
        BlockKind::ToolCall => "tool_call",
    }
}

/// A committed assistant message, flattened for a script.
fn message_map(cx: &Cx<'_>, message: MessageId) -> Map {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = rhai::Array::new();

    for content in cx.transcript().message(message).content() {
        match content {
            ContentRef::Text(part) => text.push_str(part.text()),
            ContentRef::Thinking(part) => thinking.push_str(part.text()),
            ContentRef::ToolCall(call) => {
                let mut entry = Map::new();
                entry.insert("id".into(), call.id().into());
                entry.insert("name".into(), call.name().into());
                entry.insert("arguments".into(), call.arguments_raw().into());
                calls.push(Dynamic::from_map(entry));
            }
            ContentRef::Image(_) => {}
        }
    }

    let mut map = Map::new();
    map.insert("text".into(), text.into());
    map.insert("thinking".into(), thinking.into());
    map.insert("tool_calls".into(), calls.into());
    map
}
