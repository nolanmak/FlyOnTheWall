//! Constrain only our known extraction shape, then independently check it.
//! This checks format, not whether a statement is supported by a meeting.
use fotw_summarize::{EXTRACTION_SCHEMA, SummarizeError};
use serde_json::Value;

#[derive(Clone, Copy)]
enum Kind {
    Object,
    Array,
    String,
    NullableString,
    Integer,
}

fn kind(schema: &Value) -> Option<Kind> {
    match schema.get("type")? {
        Value::String(ty) => match ty.as_str() {
            "object" => Some(Kind::Object),
            "array" => Some(Kind::Array),
            "string" => Some(Kind::String),
            "integer" => Some(Kind::Integer),
            _ => None,
        },
        Value::Array(types) if types.len() == 2 && types[0] == "string" && types[1] == "null" => {
            Some(Kind::NullableString)
        }
        _ => None,
    }
}

pub(super) fn supports(schema: &Value) -> bool {
    schema == &*EXTRACTION_SCHEMA && definition_supported(schema)
}

fn definition_supported(schema: &Value) -> bool {
    let (Some(object), Some(kind)) = (schema.as_object(), kind(schema)) else {
        return false;
    };
    let allowed: &[&str] = match kind {
        Kind::Object => &["properties", "required", "additionalProperties"],
        Kind::Array => &["items", "minItems"],
        Kind::String => &["enum"],
        Kind::NullableString => &[],
        Kind::Integer => &["minimum"],
    };
    // A future schema constraint must receive explicit native and client
    // support; it cannot silently turn into an unenforced hint.
    if object
        .keys()
        .any(|key| key != "type" && key != "description" && !allowed.contains(&key.as_str()))
    {
        return false;
    }
    match kind {
        Kind::Object => {
            let (Some(properties), Some(required)) = (
                schema["properties"].as_object(),
                schema["required"].as_array(),
            ) else {
                return false;
            };
            schema["additionalProperties"] == false
                && required.len() == properties.len()
                && properties.iter().all(|(name, child)| {
                    required
                        .iter()
                        .filter(|key| key.as_str() == Some(name))
                        .count()
                        == 1
                        && definition_supported(child)
                })
        }
        Kind::Array => {
            definition_supported(&schema["items"])
                && schema.get("minItems").is_none_or(|n| n.as_u64().is_some())
        }
        Kind::String => schema.get("enum").is_none_or(|choices| {
            choices
                .as_array()
                .is_some_and(|choices| !choices.is_empty() && choices.iter().all(Value::is_string))
        }),
        Kind::NullableString => true,
        Kind::Integer => schema.get("minimum").is_none_or(|n| n.as_u64().is_some()),
    }
}

pub(super) fn validate(text: &str, schema: &Value) -> Result<(), SummarizeError> {
    let valid = supports(schema)
        && serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|value| matches(&value, schema));
    if valid {
        Ok(())
    } else {
        // Native output can contain private meeting text. Never echo it.
        Err(SummarizeError::SchemaViolation(
            "local extraction response did not match the supported schema".into(),
        ))
    }
}

fn matches(value: &Value, schema: &Value) -> bool {
    match kind(schema) {
        Some(Kind::Object) => value.as_object().is_some_and(|object| {
            schema["properties"].as_object().is_some_and(|properties| {
                object.len() == properties.len()
                    && properties.iter().all(|(key, child)| {
                        object.get(key).is_some_and(|value| matches(value, child))
                    })
            })
        }),
        Some(Kind::Array) => value.as_array().is_some_and(|items| {
            u64::try_from(items.len())
                .is_ok_and(|len| len >= schema["minItems"].as_u64().unwrap_or(0))
                && items.iter().all(|item| matches(item, &schema["items"]))
        }),
        Some(Kind::String) => {
            value.is_string()
                && schema.get("enum").is_none_or(|choices| {
                    choices
                        .as_array()
                        .is_some_and(|choices| choices.contains(value))
                })
        }
        Some(Kind::NullableString) => value.is_null() || value.is_string(),
        Some(Kind::Integer) => value
            .as_u64()
            .is_some_and(|n| n >= schema["minimum"].as_u64().unwrap_or(0)),
        None => false,
    }
}
