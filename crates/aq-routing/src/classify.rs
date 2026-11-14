//! Classification of a task, produced by the router, its answer schema, and
//! what Settings offers to route to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Class;

/// Largest length, in characters, of a classification's reason strings.
pub const REASON_MAX_CHARS: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl Risk {
    pub const ALL: [Risk; 4] = [Risk::Low, Risk::Medium, Risk::High, Risk::VeryHigh];

    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
            Risk::VeryHigh => "very_high",
        }
    }
}

/// A yes/no fact about a task that a policy rule can require.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Narrow,
    TestVerified,
    DeepReasoning,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    pub task_type: String,
    pub intelligence_class: Class,
    pub deep_reasoning: bool,
    pub narrow: bool,
    pub test_verified: bool,
    pub risk: Risk,
    pub risk_reason: String,
    pub reason: String,
}

/// JSON Schema (draft 2020-12) of a [`Classification`] answer, limited to
/// `allowed_kinds` and `allowed_classes`.
pub fn schema(allowed_kinds: &[String], allowed_classes: &[Class]) -> Value {
    let classes: Vec<String> = allowed_classes.iter().map(Class::to_string).collect();
    let risks: Vec<&str> = Risk::ALL.iter().map(|r| r.as_str()).collect();
    let reason = json!({"type": "string", "maxLength": REASON_MAX_CHARS});
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "task_type", "intelligence_class", "deep_reasoning", "narrow",
            "test_verified", "risk", "risk_reason", "reason"
        ],
        "properties": {
            "task_type": {"type": "string", "enum": allowed_kinds},
            "intelligence_class": {"type": "string", "enum": classes},
            "deep_reasoning": {"type": "boolean"},
            "narrow": {"type": "boolean"},
            "test_verified": {"type": "boolean"},
            "risk": {"type": "string", "enum": risks},
            "risk_reason": reason,
            "reason": reason,
        }
    })
}

/// What Settings currently offers: only classes with an assigned profile, each
/// with every pair its profile lists.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub classes: BTreeMap<Class, Vec<Offer>>,
}

/// One (provider, harness) pair a class's profile lists among its candidates,
/// whether or not a worker is qualified for that harness now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub provider: String,
    pub harness: String,
}

impl Options {
    /// Every offer of every class.
    pub fn offers(&self) -> impl Iterator<Item = &Offer> {
        self.classes.values().flatten()
    }

    /// What a routing document's author is told Settings offers, as named
    /// facts: `levels` from the lowest up, `offers` with each level's
    /// harnesses and providers, and every harness and provider any level has.
    pub fn facts(&self) -> BTreeMap<String, serde_json::Value> {
        use serde_json::json;
        use std::collections::BTreeSet;
        let names = |offers: &[Offer], pick: fn(&Offer) -> &String| -> BTreeSet<String> {
            offers.iter().map(|o| pick(o).clone()).collect()
        };
        let all: Vec<Offer> = self.offers().cloned().collect();
        let levels: Vec<String> = self.classes.keys().map(Class::to_string).collect();
        let offers: serde_json::Map<String, serde_json::Value> = (self.classes.iter())
            .map(|(class, offers)| {
                let harnesses = names(offers, |o| &o.harness);
                let providers = names(offers, |o| &o.provider);
                let offered = json!({"harnesses": harnesses, "providers": providers});
                (class.to_string(), offered)
            })
            .collect();
        BTreeMap::from([
            ("levels".to_owned(), json!(levels)),
            ("offers".to_owned(), json!(offers)),
            ("harnesses".to_owned(), json!(names(&all, |o| &o.harness))),
            ("providers".to_owned(), json!(names(&all, |o| &o.provider))),
        ])
    }
}

/// A profile's offers: each (provider, harness) pair its candidates list, once,
/// in candidate order. Whether a worker is qualified for a harness now is not
/// asked: that is placement's question (owner, 2026-11-13).
pub fn offers<'a>(candidates: impl IntoIterator<Item = (&'a str, &'a [String])>) -> Vec<Offer> {
    let mut out: Vec<Offer> = Vec::new();
    for (provider, harnesses) in candidates {
        for harness in harnesses {
            let offer = Offer {
                provider: provider.into(),
                harness: harness.clone(),
            };
            if !out.contains(&offer) {
                out.push(offer);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(s: &str) -> Class {
        s.parse().unwrap()
    }

    #[test]
    fn classification_schema_limits_kinds_and_classes_and_rejects_extra_fields() {
        let kinds = vec!["bugfix".to_string(), "feature".to_string()];
        let s = schema(&kinds, &[class("standard-high"), class("complex-high")]);
        assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        let props = s["properties"].as_object().unwrap();
        let mut names: Vec<&String> = props.keys().collect();
        names.sort();
        let mut required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort();
        assert_eq!(names, required);
        assert_eq!(
            required,
            [
                "deep_reasoning",
                "intelligence_class",
                "narrow",
                "reason",
                "risk",
                "risk_reason",
                "task_type",
                "test_verified"
            ]
        );
        assert_eq!(props["task_type"]["enum"], json!(["bugfix", "feature"]));
        assert_eq!(
            props["intelligence_class"]["enum"],
            json!(["standard-high", "complex-high"])
        );
        assert_eq!(
            props["risk"]["enum"],
            json!(["low", "medium", "high", "very_high"])
        );
        for flag in ["deep_reasoning", "narrow", "test_verified"] {
            assert_eq!(props[flag], json!({"type": "boolean"}));
        }
        for text in ["risk_reason", "reason"] {
            assert_eq!(props[text], json!({"type": "string", "maxLength": 400}));
        }
    }

    #[test]
    fn every_risk_in_the_schema_deserializes_and_an_extra_field_is_refused() {
        for risk in Risk::ALL {
            let answer = json!({
                "task_type": "bugfix", "intelligence_class": "standard-high",
                "deep_reasoning": false, "narrow": true, "test_verified": true,
                "risk": risk.as_str(), "risk_reason": "r", "reason": "why"
            });
            let parsed: Classification = serde_json::from_value(answer.clone()).unwrap();
            assert_eq!(parsed.risk, risk);
            let mut extra = answer;
            extra["confidence"] = json!(0.9);
            assert!(serde_json::from_value::<Classification>(extra).is_err());
        }
    }

    #[test]
    fn options_serialize_with_classes_as_keys() {
        let options = Options {
            classes: BTreeMap::from([(
                class("standard-high"),
                vec![Offer {
                    provider: "anthropic".into(),
                    harness: "claude".into(),
                }],
            )]),
        };
        let value = serde_json::to_value(&options).unwrap();
        assert_eq!(
            value,
            json!({"classes": {"standard-high": [{"provider": "anthropic", "harness": "claude"}]}})
        );
        assert_eq!(serde_json::from_value::<Options>(value).unwrap(), options);
    }

    #[test]
    fn offers_keep_every_listed_harness_once_in_candidate_order() {
        let a = vec!["claude".to_string(), "cursor".to_string()];
        let b = vec!["codex".to_string(), "claude".to_string()];
        let candidates = [
            ("anthropic", a.as_slice()),
            ("openai", b.as_slice()),
            ("anthropic", a.as_slice()),
        ];
        let pairs: Vec<(String, String)> = offers(candidates)
            .into_iter()
            .map(|o| (o.provider, o.harness))
            .collect();
        assert_eq!(
            pairs,
            [
                ("anthropic".to_string(), "claude".to_string()),
                ("anthropic".to_string(), "cursor".to_string()),
                ("openai".to_string(), "codex".to_string()),
                ("openai".to_string(), "claude".to_string()),
            ]
        );
    }
}
