//! Configuration validation.
//!
//! A component declares a JSON Schema; configuration is checked against it
//! before `apply` runs. Bad configuration puts the fiber in `FAILED` with the
//! offending field named, which is the point: a component never starts half
//! configured, and the operator is told which line to fix rather than watching
//! something misbehave.
//!
//! Schemas are written by hand, as JSON, the way tool parameter schemas
//! already are in this harness.

use serde_json::Value;

/// Check `config` against `schema`.
///
/// # Errors
///
/// A message naming the field and what was wrong with it. Every failing
/// keyword is reported, not just the first, so one pass fixes the file.
pub fn validate(schema: &Value, config: &Value) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("the schema itself is not valid: {error}"))?;

    let problems: Vec<String> = validator
        .iter_errors(config)
        .map(|error| {
            let path = error.instance_path().to_string();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect();

    if problems.is_empty() {
        return Ok(());
    }
    Err(problems.join("; "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::validate;

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "greeting": { "type": "string" },
                "targets": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["targets"]
        })
    }

    #[test]
    fn valid_config_passes() {
        let config = json!({ "greeting": "Hello", "targets": ["alpha"] });
        assert!(validate(&schema(), &config).is_ok());
    }

    #[test]
    fn a_wrong_type_names_the_field() {
        let config = json!({ "targets": "not-an-array" });
        let error = validate(&schema(), &config).expect_err("should be refused");
        assert!(error.contains("targets"), "{error}");
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let error = validate(&schema(), &json!({})).expect_err("should be refused");
        assert!(error.contains("targets"), "{error}");
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" }, "b": { "type": "string" } }
        });
        let error = validate(&schema, &json!({ "a": 1, "b": 2 })).expect_err("refused");
        assert!(error.contains("/a") && error.contains("/b"), "{error}");
    }

    #[test]
    fn a_component_with_no_schema_is_not_this_functions_problem() {
        // `true` is the schema that accepts everything, which is what a
        // component saying nothing about its config amounts to.
        assert!(validate(&json!(true), &json!({ "anything": 1 })).is_ok());
    }
}
