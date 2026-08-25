//! The UI-agnostic widget tree a Rhai surface returns.
//!
//! A surface's `render` function returns either unit, which closes the surface,
//! or a map shaped like a small widget tree. This module turns that map into
//! [`Widget`] without knowing anything about how a host will draw it. The
//! terminal UI in `aphid-code` maps the same type to `ratatui` widgets; another
//! host could map it somewhere else entirely.

use rhai::{Array, Dynamic, Map};

/// One widget in a plugin surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Widget {
    /// A vertical stack.
    Rows { children: Vec<Widget> },
    /// A horizontal stack.
    Cols { children: Vec<Widget> },
    /// Plain text.
    Text { id: Option<String>, text: String },
    /// A selectable list.
    List {
        id: Option<String>,
        items: Vec<String>,
        selected: usize,
    },
    /// A single-line text field. The plugin owns the text through its state.
    Input {
        id: Option<String>,
        text: String,
        placeholder: String,
    },
    /// A labelled button. Clicks are reported to the surface callback.
    Button { id: Option<String>, label: String },
    /// Empty space.
    Spacer,
}

/// Read a surface `render` return value into a [`Widget`] tree.
///
/// # Errors
///
/// Returns a precise reason when the value is not a map, names an unknown
/// widget, or has malformed fields.
pub(crate) fn parse(value: &Dynamic) -> Result<Widget, String> {
    let map = value
        .clone()
        .try_cast::<Map>()
        .ok_or_else(|| "a widget must be a map".to_owned())?;
    parse_map(&map)
}

fn parse_map(map: &Map) -> Result<Widget, String> {
    let kind = map
        .get("type")
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| "a widget needs a `type`".to_owned())?;

    match kind.as_str() {
        "rows" => Ok(Widget::Rows {
            children: children(map, &kind)?,
        }),
        "cols" => Ok(Widget::Cols {
            children: children(map, &kind)?,
        }),
        "text" => Ok(Widget::Text {
            id: optional_id(map)?,
            text: string(map, "text", "text")?,
        }),
        "list" => {
            let selected = integer(map, "selected", 0)?;
            if selected < 0 {
                return Err("`selected` cannot be negative".to_owned());
            }
            Ok(Widget::List {
                id: optional_id(map)?,
                items: strings(map, "items")?,
                selected: selected as usize,
            })
        }
        "input" => Ok(Widget::Input {
            id: optional_id(map)?,
            text: string(map, "text", "input")?,
            placeholder: string(map, "placeholder", "input").unwrap_or_default(),
        }),
        "button" => Ok(Widget::Button {
            id: optional_id(map)?,
            label: string(map, "label", "button")?,
        }),
        "spacer" => Ok(Widget::Spacer),
        other => Err(format!("unknown widget type `{other}`")),
    }
}

fn children(map: &Map, kind: &str) -> Result<Vec<Widget>, String> {
    let Some(value) = map.get("children") else {
        return Err(format!("`{kind}` needs `children`"));
    };
    let values: Vec<Dynamic> = value
        .clone()
        .try_cast::<Array>()
        .ok_or_else(|| format!("`{kind}` needs `children` as an array"))?
        .into_iter()
        .collect();

    values.iter().map(parse).collect()
}

fn optional_id(map: &Map) -> Result<Option<String>, String> {
    match map.get("id") {
        None => Ok(None),
        Some(value) if value.is_unit() => Ok(None),
        Some(value) if value.is_string() => Ok(Some(value.to_string())),
        Some(_) => Err("`id` must be a string".to_owned()),
    }
}

fn string(map: &Map, field: &str, widget: &str) -> Result<String, String> {
    map.get(field)
        .filter(|value| value.is_string())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| format!("`{widget}` needs a string `{field}`"))
}

fn strings(map: &Map, field: &str) -> Result<Vec<String>, String> {
    let Some(value) = map.get(field) else {
        return Err(format!("`list` needs `{field}`"));
    };
    let values: Vec<Dynamic> = value
        .clone()
        .try_cast::<Array>()
        .ok_or_else(|| format!("`list` needs `{field}` as an array"))?
        .into_iter()
        .collect();

    values
        .iter()
        .map(|value| {
            value
                .is_string()
                .then(|| value.to_string())
                .ok_or_else(|| format!("`{field}` must contain only strings"))
        })
        .collect()
}

fn integer(map: &Map, field: &str, default: i64) -> Result<i64, String> {
    match map.get(field) {
        None => Ok(default),
        Some(value) if value.is_unit() => Ok(default),
        Some(value) => value
            .as_int()
            .map_err(|_| format!("`{field}` must be an integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rhai(source: &str) -> Result<Widget, String> {
        let engine = rhai::Engine::new();
        let value: Dynamic = engine.eval(source).expect("eval");
        parse(&value)
    }

    #[test]
    fn parses_a_nested_tree() {
        let widget = parse_rhai(
            r#"
            #{
                type: "rows",
                children: [
                    #{ type: "text", text: "hello" },
                    #{ type: "cols", children: [
                        #{ type: "button", id: "ok", label: "OK" },
                        #{ type: "spacer" }
                    ] }
                ]
            }
            "#,
        )
        .expect("parses");

        assert_eq!(
            widget,
            Widget::Rows {
                children: vec![
                    Widget::Text {
                        id: None,
                        text: "hello".into()
                    },
                    Widget::Cols {
                        children: vec![
                            Widget::Button {
                                id: Some("ok".into()),
                                label: "OK".into()
                            },
                            Widget::Spacer,
                        ]
                    }
                ]
            }
        );
    }

    #[test]
    fn a_list_needs_string_items_and_a_non_negative_selection() {
        let ok =
            parse_rhai(r#"#{ type: "list", items: ["a", "b"], selected: 1 }"#).expect("parses");
        assert_eq!(
            ok,
            Widget::List {
                id: None,
                items: vec!["a".into(), "b".into()],
                selected: 1
            }
        );

        assert!(parse_rhai(r#"#{ type: "list", items: [1] }"#).is_err());
        assert!(parse_rhai(r#"#{ type: "list", items: [], selected: -1 }"#).is_err());
    }

    #[test]
    fn malformed_widgets_are_refused() {
        assert!(parse_rhai(r#""text""#).is_err());
        assert!(parse_rhai(r#"#{ type: "mystery" }"#).is_err());
        assert!(parse_rhai(r#"#{ type: "rows" }"#).is_err());
        assert!(parse_rhai(r#"#{ type: "text", text: 3 }"#).is_err());
        assert!(parse_rhai(r#"#{ type: "button", id: 3, label: "x" }"#).is_err());
    }
}
