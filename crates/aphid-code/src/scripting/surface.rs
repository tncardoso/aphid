//! Interactive surfaces a plugin registers, written in Rhai.
//!
//! A surface is a small app of its own: a model, a function that changes it,
//! and a function that draws it. A script registers one while its body runs at
//! load time:
//!
//! ```rhai
//! surface(#{
//!     name: "todos",
//!     placement: #{ kind: "side", side: "right" },
//!     init: || #{ items: [], open: false },
//!     update: |state, msg| {
//!         if msg.kind == "key" && msg.code == "down" { state.chosen += 1; }
//!         state
//!     },
//!     view: |state| {
//!         if !state.open { return (); }
//!         #{ type: "list", items: state.items, selected: state.chosen }
//!     }
//! });
//! ```
//!
//! `init` runs once, at load, and its keys are the defaults: whatever was
//! persisted wins over them, so a surface never has to write out the `if "x" in
//! s` dance that reading a bare map needs.
//!
//! `update` is pure. It takes the model and a message and returns the new
//! model, or `#{ state: …, cmd: [ … ] }` to ask the host for something as well.
//! `view` is pure too: it takes the model and returns a widget tree, or unit to
//! close the surface.
//!
//! The plugin owns the surface's content and behaviour. The host owns where the
//! surface is placed and how its widget tree is drawn.

use aphid_core::Json;
use rhai::{Dynamic, FnPtr, Map};

use super::widget::{self, Widget};

/// Which edge a side surface sits on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Side {
    Right,
    Left,
}

impl Side {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "right" => Some(Self::Right),
            "left" => Some(Self::Left),
            _ => None,
        }
    }
}

/// Where a surface is placed.
///
/// Only [`Placement::Side`] is available today. The parser recognizes the
/// reserved placement names and reports them as unavailable rather than as
/// unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Side(Side),
}

/// A surface declaration collected while a script loaded.
#[derive(Clone)]
pub struct SurfaceSpec {
    pub name: String,
    pub placement: Placement,
    /// The model's defaults, run once at load.
    pub init: Option<FnPtr>,
    /// The model and a message in, the new model out.
    pub update: Option<FnPtr>,
    /// The model in, a widget tree out.
    pub view: FnPtr,
    /// Whether the surface asked to hear the background tick.
    pub tick: bool,
}

/// What a surface's `on_event` callback asks the host to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SurfaceAction {
    /// The event was handled; redraw.
    Consume,
    /// Redraw, then return focus to the input box.
    ReleaseFocus,
    /// Show a notice to the user.
    Notice(String),
    /// Send text to the model, as a typed line would be.
    Prompt(String),
    /// Send the surface a message of its own.
    ///
    /// What a command is in this architecture: an update decides what should
    /// happen next and says so, rather than doing it in the middle of working
    /// out the new model.
    Send { name: String, payload: Json },
}

/// The result of asking a surface to render.
#[derive(Clone, Debug)]
pub enum SurfaceRender {
    /// `render` returned unit: the surface is closed.
    Closed,
    /// `render` returned a valid widget tree.
    Widget(Widget),
    /// `render` or widget parsing failed. The surface stays open with an error
    /// placeholder.
    Failed(String),
}

/// A normalized UI event, sent to a surface's `on_event` callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceEvent {
    Key {
        code: String,
        modifiers: Vec<String>,
    },
    /// The background tick, for a surface that asked for it with `tick: true`.
    Tick,
    /// A message the surface sent itself.
    Msg {
        name: String,
        payload: Json,
    },
    Mouse {
        button: String,
        row: u16,
        column: u16,
        target: Option<String>,
    },
    Paste {
        text: String,
    },
}

impl SurfaceEvent {
    fn into_map(self) -> Map {
        match self {
            Self::Key { code, modifiers } => {
                let mut map = Map::new();
                map.insert("kind".into(), "key".into());
                map.insert("code".into(), code.into());
                map.insert(
                    "modifiers".into(),
                    modifiers
                        .into_iter()
                        .map(Dynamic::from)
                        .collect::<Vec<_>>()
                        .into(),
                );
                map
            }
            Self::Mouse {
                button,
                row,
                column,
                target,
            } => {
                let mut map = Map::new();
                map.insert("kind".into(), "mouse".into());
                map.insert("button".into(), button.into());
                map.insert("row".into(), i64::from(row).into());
                map.insert("column".into(), i64::from(column).into());
                map.insert("target".into(), target.map_or(Dynamic::UNIT, Dynamic::from));
                map
            }
            Self::Paste { text } => {
                let mut map = Map::new();
                map.insert("kind".into(), "paste".into());
                map.insert("text".into(), text.into());
                map
            }
            Self::Tick => {
                let mut map = Map::new();
                map.insert("kind".into(), "tick".into());
                map
            }
            Self::Msg { name, payload } => {
                let mut map = Map::new();
                map.insert("kind".into(), "msg".into());
                map.insert("name".into(), name.into());
                map.insert("payload".into(), super::convert::to_dynamic(&payload));
                map
            }
        }
    }
}

/// One surface as the host sees it.
#[derive(Clone)]
pub struct RegisteredSurface {
    /// Which loaded plugin owns it.
    pub plugin: String,
    /// The name the script asked for.
    pub name: String,
    pub placement: Placement,
    /// Whether the surface has an `update` callback and can take focus.
    pub interactive: bool,
    /// What to call to draw it. Carried so a caller with a listing can act on
    /// it without going back to the registry.
    pub spec: SurfaceSpec,
}

impl std::fmt::Debug for RegisteredSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredSurface")
            .field("plugin", &self.plugin)
            .field("name", &self.name)
            .field("placement", &self.placement)
            .field("interactive", &self.interactive)
            .finish()
    }
}

/// Read a `surface` declaration.
///
/// # Errors
///
/// Returns the reason it was refused, which the script sees as a runtime error
/// during plugin load.
pub(crate) fn spec(declaration: &Map) -> Result<SurfaceSpec, String> {
    let name = declaration
        .get("name")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "a surface needs a `name`".to_owned())?;

    if name.is_empty() || name.split_whitespace().count() > 1 {
        return Err(format!("`{name}` is not a usable surface name"));
    }

    let placement = placement(declaration.get("placement"), &name)?;

    // Named rather than reported as missing: a surface written for the older
    // shape says exactly what to rename, instead of "needs a `view`".
    if declaration.contains_key("render") {
        return Err(format!(
            "surface `{name}` has a `render`; the name for it is now `view`, \
             and `on_event` is now `update(state, msg)`"
        ));
    }
    if declaration.contains_key("on_event") {
        return Err(format!(
            "surface `{name}` has an `on_event`; it is now `update(state, msg)`, \
             which returns the new state"
        ));
    }

    let view = declaration
        .get("view")
        .and_then(|value| value.clone().try_cast::<FnPtr>())
        .ok_or_else(|| format!("surface `{name}` needs a `view` function"))?;

    let init = function(declaration, "init", &name)?;
    let update = function(declaration, "update", &name)?;
    let tick = declaration
        .get("tick")
        .is_some_and(|value| value.as_bool().unwrap_or(false));

    Ok(SurfaceSpec {
        name,
        placement,
        init,
        update,
        view,
        tick,
    })
}

/// An optional function in a declaration.
fn function(declaration: &Map, key: &str, surface: &str) -> Result<Option<FnPtr>, String> {
    match declaration.get(key) {
        None => Ok(None),
        Some(value) if value.is_unit() => Ok(None),
        Some(value) => value
            .clone()
            .try_cast::<FnPtr>()
            .map(Some)
            .ok_or_else(|| format!("surface `{surface}` needs `{key}` as a function")),
    }
}

fn placement(value: Option<&Dynamic>, surface: &str) -> Result<Placement, String> {
    let Some(value) = value else {
        return Err(format!("surface `{surface}` needs a `placement`"));
    };
    let map = value
        .clone()
        .try_cast::<Map>()
        .ok_or_else(|| format!("surface `{surface}` needs `placement` as a map"))?;

    let kind = map
        .get("kind")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| format!("surface `{surface}` needs a `placement.kind`"))?;

    if kind != "side" {
        return Err(format!(
            "surface `{surface}` placement `{kind}` is reserved and not available yet"
        ));
    }

    let side = map
        .get("side")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| format!("surface `{surface}` needs a `placement.side`"))?;

    let side = Side::parse(&side).ok_or_else(|| {
        if side == "bottom" {
            format!("surface `{surface}` side `bottom` is reserved and not available yet")
        } else {
            format!("surface `{surface}` needs `side: \"right\"` or `side: \"left\"`")
        }
    })?;

    Ok(Placement::Side(side))
}

/// Read what an event handler returned.
#[must_use]
pub fn actions(value: &Dynamic) -> Vec<SurfaceAction> {
    if value.is_unit() {
        return Vec::new();
    }

    if value.is_array() {
        return value
            .clone()
            .into_array()
            .unwrap_or_default()
            .iter()
            .flat_map(actions)
            .collect();
    }

    if value.is_map() {
        let map: Map = value.clone().cast();
        let kind = map.get("verdict").map(std::string::ToString::to_string);
        let text = map
            .get("reason")
            .map(std::string::ToString::to_string)
            .unwrap_or_default();

        return match kind.as_deref() {
            Some("notice") => vec![SurfaceAction::Notice(text)],
            Some("consume") => vec![SurfaceAction::Consume],
            Some("release_focus") => vec![SurfaceAction::ReleaseFocus],
            Some("prompt") => vec![SurfaceAction::Prompt(text)],
            Some("send") => vec![SurfaceAction::Send {
                name: text,
                payload: map
                    .get("payload")
                    .map_or(Json::Null, super::convert::to_json),
            }],
            _ => Vec::new(),
        };
    }

    if value.is_string() {
        return match value.to_string().as_str() {
            "consume" => vec![SurfaceAction::Consume],
            "release_focus" => vec![SurfaceAction::ReleaseFocus],
            _ => Vec::new(),
        };
    }

    Vec::new()
}

/// Every surface on offer.
///
/// Read from the registry, not from the files that compiled: a component that
/// is waiting on a service, or was unloaded, has offered nothing.
#[must_use]
pub fn registered(
    surfaces: &crate::registries::Registry<crate::registries::Surface>,
) -> Vec<RegisteredSurface> {
    surfaces
        .entries()
        .into_iter()
        .map(|entry| RegisteredSurface {
            plugin: entry.source,
            name: entry.spec.name.clone(),
            placement: entry.spec.placement.clone(),
            interactive: entry.spec.update.is_some(),
            spec: entry.spec,
        })
        .collect()
}

/// The surfaces that asked to hear the background tick.
#[must_use]
pub fn ticking(
    surfaces: &crate::registries::Registry<crate::registries::Surface>,
) -> Vec<(String, String)> {
    surfaces
        .entries()
        .into_iter()
        .filter(|entry| entry.spec.tick)
        .map(|entry| (entry.source, entry.spec.name))
        .collect()
}

impl super::host::PluginHost {
    /// The in-memory state version of one plugin, by name.
    #[must_use]
    pub fn state_version(&self, plugin: &str) -> Option<u64> {
        self.plugins()
            .iter()
            .find(|p| p.name() == plugin)
            .map(|p| p.state_version())
    }

    /// The in-memory state of one plugin, by name.
    ///
    /// `None` when no plugin with that name is loaded. The map is a copy, so a
    /// caller holding it cannot mutate the store behind the host's back.
    #[must_use]
    pub fn state_of(&self, plugin: &str) -> Option<Map> {
        self.plugins()
            .iter()
            .find(|p| p.name() == plugin)
            .map(|p| p.state())
    }

    /// Draw one surface by calling its `view` with its own model.
    ///
    /// `None` when no plugin owns that surface. Runs the script on the calling
    /// thread, which is the script thread and nothing else.
    #[must_use]
    pub fn render_surface(&self, spec: &SurfaceSpec, plugin: &str) -> Option<SurfaceRender> {
        let found = self.plugins().iter().find(|p| p.name() == plugin)?;
        let name = &spec.name;

        match found.call_fn(&spec.view, (found.surface_state(name),)) {
            Ok(value) if value.is_unit() => Some(SurfaceRender::Closed),
            Ok(value) => Some(match widget::parse(&value) {
                Ok(widget) => SurfaceRender::Widget(widget),
                Err(error) => {
                    found.report(&format!("surface `{name}` render failed: {error}"));
                    SurfaceRender::Failed(error)
                }
            }),
            Err(error) => {
                found.report(&format!("surface `{name}` render failed: {error}"));
                Some(SurfaceRender::Failed(error))
            }
        }
    }

    /// One whole step of a surface's own loop: model and message in, new model
    /// stored, actions out.
    ///
    /// `None` when no plugin owns that surface. A surface without an `update`
    /// has nothing to say to a message and keeps the model it had.
    #[must_use]
    pub fn surface_event(
        &self,
        spec: &SurfaceSpec,
        plugin: &str,
        event: SurfaceEvent,
    ) -> Option<Vec<SurfaceAction>> {
        let found = self.plugins().iter().find(|p| p.name() == plugin)?;
        let name = &spec.name;
        let Some(body) = spec.update.clone() else {
            return Some(Vec::new());
        };

        let state = found.surface_state(name);
        match found.call_fn(&body, (state, event.into_map())) {
            Ok(value) => {
                let (state, cmd) = step(&value);
                if let Some(state) = state {
                    found.set_surface_state(name, state);
                }
                Some(cmd)
            }
            Err(error) => {
                found.report(&format!("surface `{name}` update failed: {error}"));
                Some(Vec::new())
            }
        }
    }
}

/// Read what an `update` returned.
///
/// A bare map is the new model. A map with a `state` key is the new model and
/// what to ask the host for. Anything else changed nothing.
fn step(value: &Dynamic) -> (Option<Map>, Vec<SurfaceAction>) {
    // A string or a list is actions alone: `"consume"` and
    // `["consume", notice("…")]` say what to do and leave the model be.
    if value.is_string() || value.is_array() {
        return (None, actions(value));
    }

    let Some(map) = value.clone().try_cast::<Map>() else {
        return (None, Vec::new());
    };

    match map.get("state") {
        Some(state) => {
            let cmd = map.get("cmd").map(actions).unwrap_or_default();
            (state.clone().try_cast::<Map>(), cmd)
        }
        // A `verdict` map is one action, not a model with a key called
        // `verdict`: returning `notice("…")` alone has to keep working.
        None if map.contains_key("verdict") => (None, actions(value)),
        None => (Some(map), Vec::new()),
    }
}
