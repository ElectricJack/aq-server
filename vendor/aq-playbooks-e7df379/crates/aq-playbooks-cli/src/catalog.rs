//! Versioned AQ host contract, shared by synthetic pilots and production adapters.
use aq_playbooks_model::*;
use std::collections::BTreeSet;

pub fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).into()).collect()
}
pub fn object(fields: &[(&str, Schema)]) -> Schema {
    Schema::object(fields.iter().cloned().map(|(k, v)| (k.into(), v)).collect())
}
pub fn enumeration(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 128,
        values: strings(values),
    }
}
pub fn text() -> Schema {
    Schema::string(4096)
}
pub fn revision() -> Schema {
    Schema::Integer {
        minimum: 0,
        maximum: i64::MAX,
    }
}
pub fn subject() -> Schema {
    object(&[("id", text()), ("revision", revision())])
}
pub fn result() -> Schema {
    object(&[("reference", text()), ("subject", subject())])
}
pub fn work_result() -> Schema {
    object(&[
        ("reference", text()),
        ("subject", subject()),
        (
            "disposition",
            enumeration(&["accepted", "changes_requested", "failed"]),
        ),
    ])
}
pub fn list(item: Schema) -> Schema {
    Schema::Array {
        items: Box::new(item),
        max_items: 100,
    }
}
pub const CONTEXT_VIEWS: &[&str] = &[
    "work",
    "project",
    "execution_options",
    "evidence",
    "health_observations",
];
pub const WORK_KINDS: &[&str] = &[
    "implementation",
    "investigation",
    "planning",
    "review",
    "repair",
    "supervisor_authoring",
    "spec_ingestion",
    "fidelity_review",
    "onboarding",
];
pub const CHANGE_VARIANTS: &[&str] = &[
    "requirements",
    "priority",
    "dependencies",
    "execution_constraints",
    "hold",
    "release_hold",
];

/// Exact executable schema bundle shared with revision-pinned production hosts.
pub const VERSION: &str = "1.0.0";
pub const REGISTRY_JSON: &str = include_str!("../../../contracts/aq-host/registry.json");
pub const MANIFEST_JSON: &str = include_str!("../../../contracts/aq-host/manifest.json");

pub fn catalog() -> Registry {
    serde_json::from_str(REGISTRY_JSON).expect("published AQ host registry")
}
/// Host catalog 2.0.0: asynchronous provider judgment and registered check
/// execution (D6). Published beside 1.0.0, whose bytes and bindings are frozen.
pub const VERSION_2: &str = "2.0.0";
pub const REGISTRY_2_JSON: &str = include_str!("../../../contracts/aq-host-2.0.0/registry.json");
pub const MANIFEST_2_JSON: &str = include_str!("../../../contracts/aq-host-2.0.0/manifest.json");
pub fn catalog_v2() -> Registry {
    serde_json::from_str(REGISTRY_2_JSON).expect("published AQ host registry 2.0.0")
}
/// Host catalog 2.1.0: candidate.prepare's `prepared` outcome also returns the
/// immutable candidate `source` OID, so a check binds the prepared source rather
/// than a caller-supplied one. Published beside the frozen 1.0.0 and 2.0.0.
pub const VERSION_2_1: &str = "2.1.0";
pub const REGISTRY_2_1_JSON: &str = include_str!("../../../contracts/aq-host-2.1.0/registry.json");
pub const MANIFEST_2_1_JSON: &str = include_str!("../../../contracts/aq-host-2.1.0/manifest.json");
pub fn catalog_v2_1() -> Registry {
    serde_json::from_str(REGISTRY_2_1_JSON).expect("published AQ host registry 2.1.0")
}
/// Host catalog 2.2.0: 2.1.0 byte-identical, plus task facts (`work.read`),
/// routes (`work.route`), structured judgments (`judgment.assess` and
/// `aq.judgment.assessed`) and `aq.work.route_needed`. Additive only, so an
/// artifact sealed on 2.1.0 pins the same contracts here.
pub const VERSION_2_2: &str = "2.2.0";
pub const REGISTRY_2_2_JSON: &str = include_str!("../../../contracts/aq-host-2.2.0/registry.json");
pub const MANIFEST_2_2_JSON: &str = include_str!("../../../contracts/aq-host-2.2.0/manifest.json");
pub fn catalog_v2_2() -> Registry {
    serde_json::from_str(REGISTRY_2_2_JSON).expect("published AQ host registry 2.2.0")
}
pub fn describe_surface() -> serde_json::Value {
    let mut manifest: serde_json::Value =
        serde_json::from_str(MANIFEST_JSON).expect("AQ host manifest");
    manifest["kind"] = "versioned_aq_host_contract".into();
    manifest["commands"] = serde_json::json!(catalog().commands.keys().collect::<Vec<_>>());
    manifest["production_adapters"] = false.into();
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_is_exact_and_closed() {
        let r = catalog();
        assert_eq!(r.commands.len(), 12);
        assert!(!r.commands.contains_key("run_command"));
        for c in r.commands.values() {
            c.input.validate_definition().unwrap();
            for schema in c.outcomes.values() {
                schema.validate_definition().unwrap();
            }
        }
        let mut value = serde_json::json!({"view":"arbitrary_sql","subject":{"id":"work","revision":1},"snapshot":"scan","page":0});
        assert!(r.commands["context.read"].input.validate(&value).is_err());
        value["view"] = "health_observations".into();
        assert!(r.commands["context.read"].input.validate(&value).is_ok());
    }
}
