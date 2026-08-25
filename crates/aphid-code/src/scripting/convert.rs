//! Moving values between `serde_json` and Rhai.
//!
//! Tool arguments, tool result details and request bodies are all JSON on the
//! wire, and all maps in a script. Rhai ships its own JSON reader, but it does
//! not go the other way and it rejects the nulls a provider will happily send,
//! so both directions live here.

use rhai::{Dynamic, Map};
use serde_json::{Number, Value};

/// A JSON value as a Rhai value.
///
/// JSON `null` becomes Rhai's unit, and a number too large for `i64` degrades to
/// a float rather than failing — a script inspecting a payload should not have
/// to care.
pub fn to_dynamic(value: &Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(flag) => Dynamic::from_bool(*flag),
        Value::Number(number) => number_to_dynamic(number),
        Value::String(text) => Dynamic::from(text.clone()),
        Value::Array(items) => Dynamic::from_array(items.iter().map(to_dynamic).collect()),
        Value::Object(fields) => {
            let map: Map = fields
                .iter()
                .map(|(key, value)| (key.as_str().into(), to_dynamic(value)))
                .collect();
            Dynamic::from_map(map)
        }
    }
}

fn number_to_dynamic(number: &Number) -> Dynamic {
    if let Some(integer) = number.as_i64() {
        return Dynamic::from_int(integer);
    }
    number.as_f64().map_or(Dynamic::UNIT, Dynamic::from_float)
}

/// A Rhai value as a JSON value.
///
/// Unit becomes `null`. A map key is a string on both sides, so nothing is lost.
/// Anything with no JSON counterpart — a function pointer, a registered type —
/// becomes its string form rather than being dropped, which keeps a mistake
/// visible in the payload instead of silently absent.
#[must_use]
pub fn to_json(value: &Dynamic) -> Value {
    if value.is_unit() {
        return Value::Null;
    }
    if let Ok(flag) = value.as_bool() {
        return Value::Bool(flag);
    }
    if let Ok(integer) = value.as_int() {
        return Value::Number(integer.into());
    }
    if let Ok(float) = value.as_float() {
        return Number::from_f64(float).map_or(Value::Null, Value::Number);
    }
    if value.is_string() {
        return Value::String(value.clone().into_string().unwrap_or_default());
    }
    if value.is_array() {
        let items = value.clone().into_array().unwrap_or_default();
        return Value::Array(items.iter().map(to_json).collect());
    }
    if value.is_map() {
        let map: Map = value.clone().cast();
        return Value::Object(
            map.iter()
                .map(|(key, value)| (key.to_string(), to_json(value)))
                .collect(),
        );
    }
    Value::String(value.to_string())
}

/// A JSON object as a Rhai map, for a payload a hook receives as a map.
///
/// A document that is not an object yields an empty map: every payload this
/// crate builds is an object, so anything else is a caller's mistake, not a
/// shape a script should have to handle.
#[must_use]
pub fn object_to_map(value: &Value) -> Map {
    match value {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| (key.as_str().into(), to_dynamic(value)))
            .collect(),
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_survives_the_round_trip() {
        let original = serde_json::json!({
            "path": "src/lib.rs",
            "limit": 200,
            "ratio": 0.5,
            "deep": true,
            "tags": ["a", "b"],
            "nested": { "x": 1 },
            "missing": null
        });

        assert_eq!(to_json(&to_dynamic(&original)), original);
    }

    #[test]
    fn a_number_too_large_for_an_integer_becomes_a_float() {
        let value = serde_json::json!(1e300);
        assert!(to_dynamic(&value).is_float());
    }

    #[test]
    fn a_value_with_no_json_counterpart_keeps_its_string_form() {
        let value = Dynamic::from(rhai::FnPtr::new("noop").expect("a name"));
        assert_eq!(to_json(&value), Value::String("Fn(noop)".to_owned()));
    }
}
