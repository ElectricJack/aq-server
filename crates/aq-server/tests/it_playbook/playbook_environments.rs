//! Spec 0017 §6.3-6.4: host catalog 2.5.0 (profile 12) and its environment
//! commands, `environment.build` and `environment.admit`.
use aq_playbook_host::model::digest;
use aq_server::playbooks::{domain::GrantScope, review_domain::*};
use serde_json::Value;
use std::collections::BTreeSet;
use uuid::Uuid;

#[test]
fn host12_is_catalog_2_5_0_and_serves_the_delivery_record() {
    let registry = CatalogProfile::Host12.registry();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.5.0/manifest.json"
    ))
    .unwrap();
    assert_eq!(manifest["version"], HOST12_VERSION);
    assert_eq!(manifest["fingerprint"], HOST12_FINGERPRINT);
    assert_eq!(manifest["supersedes"]["fingerprint"], HOST10_FINGERPRINT);
    assert_eq!(manifest["includes"]["fingerprint"], HOST11_FINGERPRINT);
    assert_eq!(
        digest(&(HOST12_VERSION, &registry)).unwrap(),
        HOST12_FINGERPRINT
    );
    let host12 = CatalogProfile::Host12;
    assert_eq!((host12.number(), host12.config_name()), (12, "host_v12"));
    assert_eq!(CatalogProfile::from_number(12), Some(host12));
    assert!(host12.is_delivery() && host12.has_routing() && host12.has_notes());
    assert!(host12.has_environments() && !CatalogProfile::Host10.has_environments());
    // A namespace of another profile is not reused.
    assert!(host12.namespace_refusal(1, Some((10, 1))).is_some());
    // It serves Host10's commands, work.activate and the environment commands.
    let served: BTreeSet<&str> = host12.commands().iter().copied().collect();
    let mut expected: BTreeSet<&str> = HOST10_COMMANDS.into_iter().collect();
    expected.extend([WORK_ACTIVATE]);
    expected.extend(ENVIRONMENT_COMMANDS);
    assert_eq!(served, expected);
    for name in ENVIRONMENT_COMMANDS {
        let contract = &registry.commands[name];
        let asynchronous = contract.asynchronous.as_ref().unwrap();
        assert_eq!(asynchronous.completion_event, "aq.environment.completed");
        assert_eq!(contract.capabilities, BTreeSet::from([name.to_string()]));
    }
    assert!(registry.events.contains_key("aq.environment.base_changed"));
    // A grant may name them; a Host10 one may not.
    let scope = GrantScope {
        workspace_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        recipients: vec![],
        capabilities: HOST10_CAPABILITIES
            .iter()
            .chain(&HOST10_DELIVERY_CAPABILITIES)
            .chain(&ENVIRONMENT_COMMANDS)
            .chain(&[WORK_ACTIVATE])
            .map(|c| c.to_string())
            .collect(),
    };
    assert!(host12.validate_scope(&scope).is_ok());
    assert!(CatalogProfile::Host10.validate_scope(&scope).is_err());
    // Every contract the built-in delivery record is authored against is
    // accepted, its 2.3.0 work.read included; 2.2.0's is not.
    let record = aq_playbook_authoring::builtin::delivery();
    let pinned = aq_playbook_authoring::request::pinned_catalog(&record).unwrap();
    let kept = aq_playbook_authoring::request::catalog_for(&record, &pinned).unwrap();
    assert_eq!(kept.commands["work.read"].version, "2.3.0");
    for contract in kept.commands.values() {
        assert!(
            aq_playbook_host::accepts_command(&registry, contract),
            "{}",
            contract.name
        );
    }
    let host9 = CatalogProfile::Host9.registry();
    assert!(!aq_playbook_host::accepts_command(
        &registry,
        &host9.commands["work.read"]
    ));
    // The pin lists the vendored files byte for byte.
    let pin: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.5.0-source.json"
    ))
    .unwrap();
    assert_eq!(pin["catalog_fingerprint"], HOST12_FINGERPRINT);
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/aq-host-catalog-2.5.0");
    let mut on_disk = vec![];
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                on_disk.push(path);
            }
        }
    }
    let files = pin["files"].as_array().unwrap();
    assert_eq!(files.len(), on_disk.len());
    for file in files {
        let bytes = std::fs::read(root.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(
            file["sha256"].as_str().unwrap(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}
