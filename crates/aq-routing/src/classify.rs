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
