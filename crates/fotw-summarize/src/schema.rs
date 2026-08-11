//! The extraction schema and its Rust shape (spec 8.5).
//!
//! Three constraints on the JSON Schema, all of them from the providers rather
//! than from taste (spec 8.5): `additionalProperties: false` on every object,
//! every field in `required`, and no `allOf`/`if`/`then` — unsupported by both
//! Anthropic strict mode and OpenAI structured outputs. A schema that violates
//! any of them is rejected at request time, so
//! [`tests::every_object_satisfies_strict_mode`] walks the whole tree rather
//! than spot-checking.
//!
//! **The nullable fields are the feature, not a concession.** Spec 8.5 calls
//! `owner` and `due` load-bearing: they are the mechanism that lets the model
//! decline to invent. "All fields required" and "owner may be null" are not in
//! tension — required means the key must be present, and `["string", "null"]`
//! means the honest value for it is available. A schema with `owner` as a bare
//! required string leaves the model no way to say "nobody took this" except to
//! name somebody.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Whether an item was stated outright or inferred.
///
/// The evidence validator downgrades to [`Confidence::Implied`] when it has to
/// null an unverifiable owner or date (spec 8.6), so this is not purely the
/// model's own judgement by the time the UI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Stated outright in the transcript.
    Explicit,
    /// Inferred, or downgraded by the validator.
    Implied,
}

/// The evidence fields every extracted item carries.
///
/// Flattened into each item rather than nested, because a nested object would
/// be one more level for the model to get wrong and one more `required`/
/// `additionalProperties` pair to keep in sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    /// What is to be done.
    pub text: String,
    /// Exact speaker label from the transcript, or `None`. **Never guessed.**
    pub owner: Option<String>,
    /// ISO-8601, resolved against the meeting date, or `None` if unstated.
    pub due: Option<String>,
    /// The literal phrase that was said, e.g. "end of next sprint".
    ///
    /// Not decoration: spec 8.6 rule 4 validates `due` by looking for
    /// `due_raw` in a cited segment, so a resolved date with no raw phrase is
    /// unverifiable by construction.
    pub due_raw: Option<String>,
    /// Stated or inferred.
    pub confidence: Confidence,
    /// Document block indices supporting the item. At least one.
    pub evidence_segment_ids: Vec<usize>,
    /// Verbatim substring of the cited segments.
    pub evidence_quote: String,
}

/// A decision the meeting reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// What was decided.
    pub text: String,
    /// Options that were discussed and not taken. Empty is normal.
    pub alternatives_considered: Vec<String>,
    /// Stated or inferred.
    pub confidence: Confidence,
    /// Document block indices supporting the item. At least one.
    pub evidence_segment_ids: Vec<usize>,
    /// Verbatim substring of the cited segments.
    pub evidence_quote: String,
}

/// A question raised and not resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestion {
    /// The question.
    pub text: String,
    /// Speaker label of whoever raised it, or `None`.
    pub raised_by: Option<String>,
    /// Stated or inferred.
    pub confidence: Confidence,
    /// Document block indices supporting the item. At least one.
    pub evidence_segment_ids: Vec<usize>,
    /// Verbatim substring of the cited segments.
    pub evidence_quote: String,
}

/// Something to come back to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowUp {
    /// What to follow up on.
    pub text: String,
    /// What it is waiting on, or `None`.
    pub blocked_on: Option<String>,
    /// Stated or inferred.
    pub confidence: Confidence,
    /// Document block indices supporting the item. At least one.
    pub evidence_segment_ids: Vec<usize>,
    /// Verbatim substring of the cited segments.
    pub evidence_quote: String,
}

/// A topic marker for the meeting's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Topic {
    /// Short label.
    pub label: String,
    /// Block index where the topic starts — resolves to a timestamp for the
    /// scrubber.
    pub start_segment_id: usize,
}

/// Everything Call B returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Extraction {
    /// Commitments made.
    pub action_items: Vec<ActionItem>,
    /// Decisions reached.
    pub decisions: Vec<Decision>,
    /// Questions left open.
    pub open_questions: Vec<OpenQuestion>,
    /// Threads to pick up later.
    pub follow_ups: Vec<FollowUp>,
    /// Topic markers.
    pub topics: Vec<Topic>,
}

impl Extraction {
    /// Total items across every category except topics.
    ///
    /// Topics are excluded because they carry no evidence fields and are not
    /// subject to the drop rules.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.action_items.len()
            + self.decisions.len()
            + self.open_questions.len()
            + self.follow_ups.len()
    }
}

/// Which list an item came from, for reporting drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// An action item.
    ActionItem,
    /// A decision.
    Decision,
    /// An open question.
    OpenQuestion,
    /// A follow-up.
    FollowUp,
    /// A topic marker. Carries no quote, but its `start_segment_id` still has
    /// to resolve or it cannot be placed on the timeline.
    Topic,
}

/// The JSON Schema sent as `output_config.format.schema` on Call B.
pub static EXTRACTION_SCHEMA: LazyLock<Value> = LazyLock::new(build_extraction_schema);

/// A `["T", "null"]` typed property with a description.
fn nullable(ty: &str, description: &str) -> Value {
    json!({ "type": [ty, "null"], "description": description })
}

/// An object with `additionalProperties: false` and every key required.
///
/// Taking `required` from `properties` rather than accepting it as an argument
/// is deliberate: spec 8.5 says *all* fields are required, and a helper that
/// let a caller pass a shorter list is a helper that lets the rule be broken
/// by omission.
fn strict_object(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The evidence fields shared by every item kind (spec 8.5).
fn evidence_properties() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "confidence".to_string(),
        json!({
            "type": "string",
            "enum": ["explicit", "implied"],
            "description": "`explicit` if stated outright, `implied` if inferred.",
        }),
    );
    map.insert(
        "evidence_segment_ids".to_string(),
        json!({
            "type": "array",
            "items": { "type": "integer", "minimum": 0 },
            "minItems": 1,
            "description":
                "The `[#N]` ids of the transcript segments supporting this item. \
                 Only ids that appear in the transcript.",
        }),
    );
    map.insert(
        "evidence_quote".to_string(),
        json!({
            "type": "string",
            "description":
                "Copied verbatim from the cited segments: the spoken words only, without the \
                 `[#N]` prefix, the speaker label or the timestamp.",
        }),
    );
    map
}

/// An item object: its own properties plus the shared evidence fields.
fn item_object(own: Vec<(&str, Value)>) -> Value {
    let mut properties = serde_json::Map::new();
    for (key, value) in own {
        properties.insert(key.to_string(), value);
    }
    properties.extend(evidence_properties());
    strict_object(Value::Object(properties))
}

fn build_extraction_schema() -> Value {
    let action_item = item_object(vec![
        ("text", json!({ "type": "string" })),
        (
            "owner",
            nullable(
                "string",
                "The exact speaker label from the transcript, or null. NEVER guess. A null \
                 owner is correct and expected when nobody took the item.",
            ),
        ),
        (
            "due",
            nullable(
                "string",
                "ISO-8601 date resolved against the meeting date, or null if no deadline was \
                 stated. Do not infer a deadline from urgency.",
            ),
        ),
        (
            "due_raw",
            nullable(
                "string",
                "The literal phrase that was said, e.g. \"end of next sprint\". Null when `due` \
                 is null.",
            ),
        ),
    ]);

    let decision = item_object(vec![
        ("text", json!({ "type": "string" })),
        (
            "alternatives_considered",
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Options discussed and not taken. An empty array is normal.",
            }),
        ),
    ]);

    let open_question = item_object(vec![
        ("text", json!({ "type": "string" })),
        (
            "raised_by",
            nullable("string", "Speaker label of whoever raised it, or null."),
        ),
    ]);

    let follow_up = item_object(vec![
        ("text", json!({ "type": "string" })),
        (
            "blocked_on",
            nullable("string", "What it is waiting on, or null."),
        ),
    ]);

    let topic = strict_object(json!({
        "label": { "type": "string" },
        "start_segment_id": { "type": "integer", "minimum": 0 },
    }));

    strict_object(json!({
        "action_items": { "type": "array", "items": action_item },
        "decisions": { "type": "array", "items": decision },
        "open_questions": { "type": "array", "items": open_question },
        "follow_ups": { "type": "array", "items": follow_up },
        "topics": { "type": "array", "items": topic },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk every object in the schema, applying `check`.
    fn walk_objects(value: &Value, path: &str, check: &mut impl FnMut(&Value, &str)) {
        match value {
            Value::Object(map) => {
                if map.get("type") == Some(&json!("object")) {
                    check(value, path);
                }
                for (key, child) in map {
                    walk_objects(child, &format!("{path}/{key}"), check);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk_objects(child, &format!("{path}/{index}"), check);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_object_satisfies_strict_mode() {
        // Spec 8.5's three hard constraints, checked on every object in the
        // tree rather than on the ones a reviewer happened to look at.
        let mut objects_seen = 0;
        walk_objects(&EXTRACTION_SCHEMA, "", &mut |object, path| {
            objects_seen += 1;
            assert_eq!(
                object.get("additionalProperties"),
                Some(&json!(false)),
                "{path} is missing additionalProperties: false"
            );
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{path} has no properties"));
            let required: Vec<&str> = object
                .get("required")
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("{path} has no required"))
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for key in properties.keys() {
                assert!(
                    required.contains(&key.as_str()),
                    "{path}: `{key}` is a property but not required"
                );
            }
            assert_eq!(
                required.len(),
                properties.len(),
                "{path}: required lists a key that is not a property"
            );
        });
        assert!(
            objects_seen >= 6,
            "the walker only found {objects_seen} objects; it is not reaching the item schemas"
        );
    }

    #[test]
    fn no_unsupported_keyword_appears_anywhere() {
        // allOf/if/then are rejected by Anthropic strict mode and OpenAI
        // structured outputs alike (spec 8.5). Checking the serialized text
        // catches them wherever they are nested.
        let text = EXTRACTION_SCHEMA.to_string();
        for keyword in [
            "\"allOf\"",
            "\"if\"",
            "\"then\"",
            "\"else\"",
            "\"not\"",
            "\"oneOf\"",
        ] {
            assert!(
                !text.contains(keyword),
                "schema contains the unsupported keyword {keyword}"
            );
        }
    }

    #[test]
    fn owner_and_due_are_nullable_and_the_description_says_never_guess() {
        // Spec 8.5 calls these load-bearing: they are how the model declines
        // to invent. A change to a bare `"type": "string"` here would silently
        // remove the model's only honest answer.
        let owner =
            &EXTRACTION_SCHEMA["properties"]["action_items"]["items"]["properties"]["owner"];
        assert_eq!(owner["type"], json!(["string", "null"]));
        let description = owner["description"].as_str().expect("description");
        assert!(description.contains("NEVER guess"));
        assert!(description.contains("null owner is correct and expected"));

        for field in ["due", "due_raw"] {
            let property =
                &EXTRACTION_SCHEMA["properties"]["action_items"]["items"]["properties"][field];
            assert_eq!(property["type"], json!(["string", "null"]), "{field}");
        }
    }

    #[test]
    fn every_nullable_field_in_the_spec_is_nullable_here() {
        let items = &EXTRACTION_SCHEMA["properties"];
        for (list, field) in [
            ("action_items", "owner"),
            ("action_items", "due"),
            ("action_items", "due_raw"),
            ("open_questions", "raised_by"),
            ("follow_ups", "blocked_on"),
        ] {
            assert_eq!(
                items[list]["items"]["properties"][field]["type"],
                json!(["string", "null"]),
                "{list}.{field} is not nullable"
            );
        }
    }

    #[test]
    fn evidence_segment_ids_requires_at_least_one_id() {
        let ids = &EXTRACTION_SCHEMA["properties"]["decisions"]["items"]["properties"]["evidence_segment_ids"];
        assert_eq!(ids["minItems"], json!(1));
        assert_eq!(ids["items"]["type"], json!("integer"));
    }

    #[test]
    fn a_meeting_with_nobody_assigned_deserializes_with_a_null_owner() {
        // The shape spec 8.5 wants back when nobody took the item: nulls, not
        // a plausible name. This is the wire contract; `crate::validate` is
        // what enforces it against a model that ignores it.
        let raw = json!({
            "action_items": [{
                "text": "Update the docs",
                "owner": null,
                "due": null,
                "due_raw": null,
                "confidence": "implied",
                "evidence_segment_ids": [3],
                "evidence_quote": "Somebody needs to update the docs"
            }],
            "decisions": [],
            "open_questions": [],
            "follow_ups": [],
            "topics": []
        });
        let extraction: Extraction = serde_json::from_value(raw).expect("deserialize");
        let item = &extraction.action_items[0];
        assert_eq!(item.owner, None);
        assert_eq!(item.due, None);
        assert_eq!(item.due_raw, None);
        assert_eq!(item.confidence, Confidence::Implied);
    }

    #[test]
    fn the_rust_types_round_trip_through_the_wire_shape() {
        // The Rust struct and the JSON Schema have to agree on field names or
        // Call B's output silently fails to deserialize in production while
        // every schema test still passes.
        let extraction = Extraction {
            action_items: vec![ActionItem {
                text: "ship it".to_string(),
                owner: Some("S0".to_string()),
                due: Some("2026-08-14".to_string()),
                due_raw: Some("by Friday".to_string()),
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![2],
                evidence_quote: "I will write the migration script by Friday".to_string(),
            }],
            decisions: vec![Decision {
                text: "SQLite".to_string(),
                alternatives_considered: vec!["Postgres".to_string()],
                confidence: Confidence::Explicit,
                evidence_segment_ids: vec![1],
                evidence_quote: "we agreed".to_string(),
            }],
            open_questions: vec![OpenQuestion {
                text: "export format?".to_string(),
                raised_by: None,
                confidence: Confidence::Implied,
                evidence_segment_ids: vec![4],
                evidence_quote: "Open question".to_string(),
            }],
            follow_ups: vec![FollowUp {
                text: "docs".to_string(),
                blocked_on: None,
                confidence: Confidence::Implied,
                evidence_segment_ids: vec![3],
                evidence_quote: "update the docs".to_string(),
            }],
            topics: vec![Topic {
                label: "migration".to_string(),
                start_segment_id: 0,
            }],
        };

        let json = serde_json::to_value(&extraction).expect("serialize");
        let schema_properties = EXTRACTION_SCHEMA["properties"]
            .as_object()
            .expect("schema properties");
        let emitted = json.as_object().expect("emitted object");
        assert_eq!(
            emitted.keys().collect::<Vec<_>>(),
            schema_properties.keys().collect::<Vec<_>>(),
            "the Rust type and the schema disagree on the top-level keys"
        );

        for (list, sample) in [
            ("action_items", &json["action_items"][0]),
            ("decisions", &json["decisions"][0]),
            ("open_questions", &json["open_questions"][0]),
            ("follow_ups", &json["follow_ups"][0]),
            ("topics", &json["topics"][0]),
        ] {
            let expected = EXTRACTION_SCHEMA["properties"][list]["items"]["properties"]
                .as_object()
                .expect("item properties");
            let actual = sample.as_object().expect("item object");
            let mut expected_keys: Vec<&String> = expected.keys().collect();
            let mut actual_keys: Vec<&String> = actual.keys().collect();
            expected_keys.sort();
            actual_keys.sort();
            assert_eq!(expected_keys, actual_keys, "{list} field names disagree");
        }

        let back: Extraction = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, extraction);
    }

    #[test]
    fn item_count_excludes_topics() {
        let extraction = Extraction {
            topics: vec![Topic {
                label: "x".to_string(),
                start_segment_id: 0,
            }],
            ..Extraction::default()
        };
        assert_eq!(extraction.item_count(), 0);
    }
}
