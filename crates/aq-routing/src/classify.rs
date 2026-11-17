//! What Settings offers to route to. (A task's classification is the routing
//! playbook's own `judgment.assess` answer; the server keeps no shape for it.)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Class;

/// What Settings currently offers: only classes with an assigned profile, each
/// with every pair its profile lists.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub classes: BTreeMap<Class, Vec<Offer>>,
    /// The name and description of each class's level document: what a
    /// routing document's author reads to tell the levels apart.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub described: BTreeMap<Class, Described>,
}

/// What a level document says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Described {
    pub name: String,
    pub description: String,
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
    /// facts: `levels` from the lowest up, `level_descriptions` with each
    /// level's name and description, `offers` with each level's harnesses
    /// and providers, and every harness and provider any level has.
    pub fn facts(&self) -> BTreeMap<String, serde_json::Value> {
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
        let described: serde_json::Map<String, serde_json::Value> = (self.described.iter())
            .map(|(class, d)| (class.to_string(), json!(d)))
            .collect();
        BTreeMap::from([
            ("levels".to_owned(), json!(levels)),
            ("level_descriptions".to_owned(), json!(described)),
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
    fn options_serialize_with_classes_as_keys() {
        let options = Options {
            classes: BTreeMap::from([(
                class("standard-high"),
                vec![Offer {
                    provider: "anthropic".into(),
                    harness: "claude".into(),
                }],
            )]),
            described: BTreeMap::new(),
        };
        let value = serde_json::to_value(&options).unwrap();
        assert_eq!(
            value,
            json!({"classes": {"standard-high": [{"provider": "anthropic", "harness": "claude"}]}})
        );
        assert_eq!(serde_json::from_value::<Options>(value).unwrap(), options);
    }

    #[test]
    fn the_facts_describe_each_level_by_its_document() {
        let level = class("standard-high");
        let options = Options {
            classes: BTreeMap::from([(level, vec![])]),
            described: BTreeMap::from([(
                level,
                Described {
                    name: "Standard, high".into(),
                    description: "Ordinary work that needs careful checking.".into(),
                },
            )]),
        };
        let facts = options.facts();
        assert_eq!(facts["levels"], json!(["standard-high"]));
        assert_eq!(
            facts["level_descriptions"],
            json!({"standard-high": {"name": "Standard, high",
                "description": "Ordinary work that needs careful checking."}})
        );
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
