//! Small helpers for hand-shaped model-facing JSON schemas.
//!
//! Most built-in tools should keep deriving `JsonSchema` from their
//! parameter structs. These helpers are for the Claude-Code-style cases
//! where we intentionally expose a flat, strict object schema inferred
//! from example values and then annotate it with field guidance.

use schemars::schema_for_value;
use serde_json::{Value, json};

/// Build a strict object schema from an example JSON object.
///
/// Optional fields are represented by being present in `properties` but
/// absent from `required`; this avoids nullable unions in provider-facing
/// schemas.
pub(crate) fn object_from_example(example: &Value, required: &[&str]) -> Value {
    let mut schema = serde_json::to_value(schema_for_value!(example))
        .expect("schema_for_value output always serializes");
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("required".to_string(), json!(required));
        obj.insert("additionalProperties".to_string(), json!(false));
    }
    schema
}

/// Build a strict empty object schema.
pub(crate) fn empty_object() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
}

/// Add a JSON Schema description annotation to one property.
pub(crate) fn describe(schema: &mut Value, name: &str, description: &str) {
    if let Some(property) = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut)
    {
        property.insert("description".to_string(), json!(description));
    }
}

/// Add a string enum constraint to one property.
pub(crate) fn enum_strings(schema: &mut Value, name: &str, variants: &[&str]) {
    if let Some(property) = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut)
    {
        property.insert("enum".to_string(), json!(variants));
    }
}
