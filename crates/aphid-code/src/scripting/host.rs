//! The bridge between the agent's hooks and the loaded scripts.
//!
//! [`PluginHost`] is one [`Plugin`] standing in for every `.rhai` file, so the
//! agent's registry sees a single subscriber however many scripts are loaded.
//! It subscribes once for all of them, and each announcement visits only the
//! scripts that define the matching hook.
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

use aphid_agent::StreamCx;
use aphid_agent::rt::{Component, Composition, Context};
use aphid_core::{BlockKind, ContentRef, Event, MessageId, StopReason, Transcript};
use rhai::{Dynamic, Map};

use aphid_agent::{Silent, Sink};

use super::caps::Capabilities;
use super::discover::{Diagnostic, PluginFile};
use super::script::ScriptPlugin;
use super::worker::Worker;

/// Every loaded plugin, as one agent plugin.
pub struct PluginHost {
    plugins: Vec<Arc<ScriptPlugin>>,
    diagnostics: Vec<Diagnostic>,
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
        processes: &Arc<aphid_agent::exec::Registry>,
    ) -> (Self, Vec<Diagnostic>) {
        let worker = Arc::new(Worker::spawn(processes));
        let mut plugins = Vec::new();
        let mut diagnostics = Vec::new();

        for file in files {
            match ScriptPlugin::load(file, caps, &sink, &worker) {
                Ok(plugin) => {
                    let plugin = Arc::new(plugin);
                    plugin.wire();
                    plugins.push(plugin);
                }
                Err(message) => diagnostics.push(Diagnostic {
                    path: file.path.clone(),
                    message,
                }),
            }
        }

        let host = Self {
            plugins,
            diagnostics: diagnostics.clone(),
        };
        (host, diagnostics)
    }

    /// A host with nothing loaded, for `--no-plugins` and for tests.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            diagnostics: Vec::new(),
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

    /// Write every plugin's state back.
    ///
    /// Not an announcement: nothing subscribes to it and nothing may refuse it.
    /// A session that ends — cleanly or not — has whatever its plugins learned
    /// on disk, and that is the host's job rather than anybody's listener.
    pub fn flush(&self) {
        for plugin in self.plugins() {
            plugin.flush();
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

/// Mounts every loaded script, one fiber each.
///
/// A parent rather than a single subscriber: each script gets its own fiber, so
/// its `inject` decides when *it* runs and its failure is its own. Unloading
/// this unloads all of them, because mounting a child is an ordinary tracked
/// effect.
pub struct ScriptHost {
    host: Arc<PluginHost>,
    composition: Composition,
}

impl ScriptHost {
    #[must_use]
    pub fn new(host: Arc<PluginHost>, composition: &Composition) -> Self {
        Self {
            host,
            composition: composition.clone(),
        }
    }
}

impl Component for ScriptHost {
    fn name(&self) -> &str {
        "scripts"
    }

    fn apply(&self, ctx: &Context) -> Result<(), String> {
        for plugin in self.host.plugins() {
            let component =
                super::component::ScriptComponent::new(Arc::clone(plugin), &self.composition);
            // A script that cannot be mounted — a cycle it would close — is
            // reported and skipped. The others still load, which is the same
            // bargain a file that does not compile already gets.
            if let Err(error) = ctx.mount(Arc::new(component), serde_json::Value::Null) {
                plugin
                    .sink()
                    .log(plugin.name(), &format!("not mounted: {error}"));
            }
        }
        Ok(())
    }
}

/// A host that loads nothing, for a caller that wants the type without the cost.
#[must_use]
pub fn silent_sink() -> Arc<dyn Sink> {
    Arc::new(Silent)
}

/// Read a `#{ verdict: …, reason: … }` map, as `block(…)` and friends build.
pub(crate) fn verdict(value: &Dynamic) -> Option<(&'static str, String)> {
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

pub(crate) fn as_map(value: &Dynamic) -> Option<Map> {
    value.is_map().then(|| value.clone().cast::<Map>())
}

pub(crate) fn field_string(value: &Dynamic, key: &str) -> Option<String> {
    let map = as_map(value)?;
    let field = map.get(key)?;
    field.is_string().then(|| field.to_string())
}

pub(crate) fn stop_reason(stop: StopReason) -> &'static str {
    match stop {
        StopReason::Pending => "pending",
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "tool_use",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

pub(crate) fn event_map(event: &Event, cx: &StreamCx<'_>) -> Map {
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
pub(crate) fn message_map(transcript: &Transcript, message: MessageId) -> Map {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = rhai::Array::new();

    for content in transcript.message(message).content() {
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
