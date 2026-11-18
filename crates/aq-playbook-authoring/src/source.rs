//! What a source is (spec 0013 §4.1), and what the host server tells about it.
use crate::Result;
use aq_internal_work::Purpose;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};

/// Whether the server defines the source (`routing`) or an owner or a
/// supervisor registered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Builtin,
    Registered,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Builtin => "builtin",
            SourceKind::Registered => "registered",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "builtin" => Some(SourceKind::Builtin),
            "registered" => Some(SourceKind::Registered),
            _ => None,
        }
    }
}

/// The record the server keeps beside a source's Markdown: its id, the
/// binding (playbook id) it activates on, the grant scope, what it is for,
/// the purpose its judgments run under, which catalog commands, events and
/// capabilities its playbook may use, and which fact providers the authoring
/// agent is shown. `revision` is the stored record's: it is read back, and
/// ignored when writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRecord {
    pub id: String,
    pub kind: SourceKind,
    pub binding: String,
    pub scope: String,
    pub purpose: String,
    pub judgment_purpose: Purpose,
    pub commands: BTreeSet<String>,
    pub events: BTreeSet<String>,
    pub capabilities: BTreeSet<String>,
    /// Fact provider names, in the order the agent is shown them.
    pub facts: Vec<String>,
    pub revision: u64,
}

impl SourceRecord {
    /// Whether `other` says the same, whatever either revision is.
    pub(crate) fn same_as(&self, other: &SourceRecord) -> bool {
        let revision = self.revision;
        let other = SourceRecord {
            revision,
            ..other.clone()
        };
        *self == other
    }
}

/// What the host server knows about a source that this crate does not keep.
pub trait Host: AsRef<PgPool> + Clone + Send + Sync + 'static {
    /// The source's Markdown now and its revision. For `routing` that is the
    /// structure document's routing text and revision; for a registered
    /// source it is [`crate::stored_markdown`].
    fn markdown(&self, source: &SourceRecord)
    -> impl Future<Output = Result<(String, u64)>> + Send;
    /// The facts the agent is shown now, by provider name.
    fn facts(
        &self,
        source: &SourceRecord,
    ) -> impl Future<Output = Result<BTreeMap<String, Value>>> + Send;
}

/// `sha256:<hex>` of the Markdown text.
pub fn source_digest(markdown: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(markdown.as_bytes()))
}

/// The compiler binds the full normalized source including its generated header.
/// Keep this representation in sync with `aq_playbooks_compiler::draft::source_text`.
pub fn artifact_source_digest(header: &aq_playbooks_model::SourceHeader, markdown: &str) -> String {
    let header =
        serde_json::to_string(header).expect("source header contains only strings and sets");
    let compiled_source = format!("---aq-playbook\n{header}\n---\n{markdown}");
    source_digest(&aq_playbooks_compiler::normalize_source(&compiled_source))
}

/// `sha256:<hex>` of everything the record says except its revision: a run
/// authored under one record is stale under another (a newer server widening
/// the built-in record, or a registered source given fewer commands).
pub fn record_digest(record: &SourceRecord) -> String {
    let value = serde_json::json!({
        "id": record.id,
        "kind": record.kind,
        "binding": record.binding,
        "scope": record.scope,
        "purpose": record.purpose,
        "judgment_purpose": record.judgment_purpose,
        "commands": record.commands,
        "events": record.events,
        "capabilities": record.capabilities,
        "facts": record.facts,
    });
    let bytes = serde_json::to_vec(&sorted(&value)).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// `sha256:<hex>` of the facts as JSON with every object's keys sorted, so the
/// digest does not depend on the order a provider built them in.
pub fn facts_digest(facts: &BTreeMap<String, Value>) -> String {
    let facts: serde_json::Map<String, Value> = (facts.iter())
        .map(|(name, value)| (name.clone(), sorted(value)))
        .collect();
    let bytes = serde_json::to_vec(&facts).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let map = keys.into_iter().map(|k| (k.clone(), sorted(&map[k])));
            Value::Object(map.collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_source_digest_is_the_sha256_of_the_text() {
        assert_eq!(
            source_digest(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(
            source_digest("Research is Standard."),
            source_digest("Research is Standard. ")
        );
    }

    #[test]
    fn the_facts_digest_ignores_key_order_and_sees_every_value() {
        let mut a = serde_json::Map::new();
        a.insert("b".into(), json!(1));
        a.insert("a".into(), json!({"y": [1, {"q": 1, "p": 2}], "x": true}));
        let mut b = serde_json::Map::new();
        b.insert("a".into(), json!({"x": true, "y": [1, {"p": 2, "q": 1}]}));
        b.insert("b".into(), json!(1));
        let one = BTreeMap::from([("options".to_string(), Value::Object(a))]);
        let two = BTreeMap::from([("options".to_string(), Value::Object(b))]);
        assert_eq!(facts_digest(&one), facts_digest(&two));
        let three = BTreeMap::from([("options".to_string(), json!({"a": 1, "b": 2}))]);
        assert_ne!(facts_digest(&one), facts_digest(&three));
        assert_ne!(facts_digest(&one), facts_digest(&BTreeMap::new()));
    }

    #[test]
    fn a_source_kind_reads_back_as_written() {
        for kind in [SourceKind::Builtin, SourceKind::Registered] {
            assert_eq!(SourceKind::parse(kind.as_str()), Some(kind));
            assert_eq!(json!(kind), json!(kind.as_str()));
        }
        assert_eq!(SourceKind::parse("Builtin"), None);
    }

    #[test]
    fn records_differing_only_in_revision_are_the_same_and_digest_alike() {
        let record = SourceRecord {
            id: "routing".into(),
            kind: SourceKind::Builtin,
            binding: "aq.routing".into(),
            scope: "installation".into(),
            purpose: "Route every task".into(),
            judgment_purpose: Purpose::RoutingClassification,
            commands: BTreeSet::from(["work.read".to_string()]),
            events: BTreeSet::new(),
            capabilities: BTreeSet::from(["work.read".to_string()]),
            facts: vec!["routing_options".into()],
            revision: 1,
        };
        let later = SourceRecord {
            revision: 7,
            ..record.clone()
        };
        assert!(record.same_as(&later));
        assert_eq!(record_digest(&record), record_digest(&later));
        let wider = SourceRecord {
            capabilities: BTreeSet::from(["work.read".into(), "work.route".into()]),
            ..later
        };
        assert!(!record.same_as(&wider));
        assert_ne!(record_digest(&record), record_digest(&wider));
        let narrower = SourceRecord {
            events: BTreeSet::from(["aq.work.route_needed".to_string()]),
            ..record.clone()
        };
        assert_ne!(record_digest(&record), record_digest(&narrower));
    }
}
