//! Interactive surfaces a plugin registers, written in Rhai.
//!
//! A script registers one while its body runs at load time:
//!
//! ```rhai
//! register_surface(#{
//!     name: "todos",
//!     placement: #{ kind: "side", side: "right" },
//!     render: |state| {
//!         if !state.open { return (); }
//!         #{ type: "text", text: "todos" }
//!     },
//!     on_event: |event| { "consume" }
//! });
//! ```
//!
//! The plugin owns the surface's content and behavior. The host owns where the
//! surface is placed and how its widget tree is drawn. `render` returns unit to
//! close the surface, or a widget tree to open it.

use std::sync::{Arc, Mutex};

use rhai::{Dynamic, FnPtr, Map};

use crate::widget::{self, Widget};

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
    pub render: FnPtr,
    pub on_event: Option<FnPtr>,
}

/// Where `register_surface` puts what it is given.
///
/// Shared with the engine's closure, because registration happens while the
/// script's body runs and the plugin does not exist yet at that point.
pub type Registry = Arc<Mutex<Vec<SurfaceSpec>>>;

/// What a surface's `on_event` callback asks the host to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SurfaceAction {
    /// The event was handled; redraw.
    Consume,
    /// Redraw, then return focus to the input box.
    ReleaseFocus,
    /// Show a notice to the user.
    Notice(String),
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
        }
    }
}

/// One surface as the host sees it.
#[derive(Clone, Debug)]
pub struct RegisteredSurface {
    /// Which loaded plugin owns it.
    pub plugin: String,
    /// The name the script asked for.
    pub name: String,
    pub placement: Placement,
    /// Whether the surface has an `on_event` callback and can take focus.
    pub interactive: bool,
}

/// Read a `register_surface` argument.
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

    let render = declaration
        .get("render")
        .and_then(|value| value.clone().try_cast::<FnPtr>())
        .ok_or_else(|| format!("surface `{name}` needs a `render` function"))?;

    let on_event = match declaration.get("on_event") {
        None => None,
        Some(value) if value.is_unit() => None,
        Some(value) => Some(
            value
                .clone()
                .try_cast::<FnPtr>()
                .ok_or_else(|| format!("surface `{name}` needs `on_event` as a function"))?,
        ),
    };

    Ok(SurfaceSpec {
        name,
        placement,
        render,
        on_event,
    })
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

impl crate::host::PluginHost {
    /// Every surface the loaded plugins registered.
    #[must_use]
    pub fn surfaces(&self) -> Vec<RegisteredSurface> {
        self.plugins()
            .iter()
            .flat_map(|plugin| {
                plugin.surfaces().iter().map(|spec| RegisteredSurface {
                    plugin: plugin.name().to_owned(),
                    name: spec.name.clone(),
                    placement: spec.placement.clone(),
                    interactive: spec.on_event.is_some(),
                })
            })
            .collect()
    }

    /// Whether any loaded plugin registered a surface.
    #[must_use]
    pub fn has_surfaces(&self) -> bool {
        self.plugins()
            .iter()
            .any(|plugin| !plugin.surfaces().is_empty())
    }

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

    /// Render one surface by calling its `render` with the plugin's state.
    ///
    /// `None` when no plugin owns that surface. Runs the script on the calling
    /// thread, so a caller that holds a UI lock must not hold it across this
    /// call.
    #[must_use]
    pub fn render_surface(&self, plugin: &str, name: &str) -> Option<SurfaceRender> {
        let found = self.plugins().iter().find(|p| p.name() == plugin)?;
        let spec = found.surfaces().iter().find(|d| d.name == name)?;

        match found.call_fn(&spec.render, (found.state(),)) {
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

    /// Deliver one UI event to a surface's `on_event` callback.
    ///
    /// `None` when no plugin owns that surface. A surface without `on_event`
    /// yields no actions.
    #[must_use]
    pub fn surface_event(
        &self,
        plugin: &str,
        name: &str,
        event: SurfaceEvent,
    ) -> Option<Vec<SurfaceAction>> {
        let found = self.plugins().iter().find(|p| p.name() == plugin)?;
        let spec = found.surfaces().iter().find(|d| d.name == name)?;
        let Some(body) = &spec.on_event else {
            return Some(Vec::new());
        };

        let event = event.into_map();
        match found.call_fn(body, (event,)) {
            Ok(value) => Some(actions(&value)),
            Err(error) => {
                found.report(&format!("surface `{name}` event failed: {error}"));
                Some(Vec::new())
            }
        }
    }
}
