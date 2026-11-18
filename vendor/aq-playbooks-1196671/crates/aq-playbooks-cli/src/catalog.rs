//! Versioned AQ host contracts, shared by the authoring tools, the test pilots
//! and production adapters. The schema builders the tests write contracts
//! with live in `tests/support/schemas.rs`.
use aq_playbooks_model::*;

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

/// Host catalog 2.3.0 (Epic 0, delivery by playbook): 2.2.0 plus the delivery
/// event `aq.work.result_ready`, its commands (`work.complete`, `work.return`,
/// `work.hold`, `main.sync`, `branch.publish`, `pull_request.*`,
/// `supervisor.notify`), the completion event `aq.delivery.completed`, and
/// `work.read` extended with the facts of a task's result. Every other 2.2.0
/// contract is byte-identical.
#[cfg(feature = "delivery_playbook")]
pub const VERSION_2_3: &str = "2.3.0";
#[cfg(feature = "delivery_playbook")]
pub const REGISTRY_2_3_JSON: &str = include_str!("../../../contracts/aq-host-2.3.0/registry.json");
#[cfg(feature = "delivery_playbook")]
pub const MANIFEST_2_3_JSON: &str = include_str!("../../../contracts/aq-host-2.3.0/manifest.json");
#[cfg(feature = "delivery_playbook")]
pub fn catalog_v2_3() -> Registry {
    serde_json::from_str(REGISTRY_2_3_JSON).expect("published AQ host registry 2.3.0")
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
