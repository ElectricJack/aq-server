use aq_playbooks_model::*;
use aq_playbooks_runner::{catalog_inspection::*, *};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn artifact(name: &str, purpose: &str) -> Artifact {
    let value = Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "catalog-test".into(),
        source: SourceBinding {
            digest: hash_bytes(name.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: name.into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: name.into(),
            purpose: purpose.into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "done".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "done".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap();
    value.verify().unwrap();
    value
}
fn authority() -> Authority {
    Authority {
        actor: "catalog-test".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    }
}

#[test]
fn exact_metadata_and_controls_are_owned_bounded_and_private_values_are_omitted() {
    let engine = Engine::new(MemoryStorage::default(), "catalog", "owner").unwrap();
    let mut value = artifact("policy", &"é".repeat(3000));
    value.content.definitions.insert(
        "private".into(),
        CapturedDefinition {
            version: "1".into(),
            digest: digest(&json!("PRIVATE_CAPTURED_SENTINEL")).unwrap(),
            value: json!("PRIVATE_CAPTURED_SENTINEL"),
        },
    );
    value
        .content
        .source
        .header
        .definitions
        .insert("private".into());
    value = Artifact::seal(value.content).unwrap();
    value.verify().unwrap();
    engine.install(value.clone()).unwrap();
    engine
        .activate("policy", &value.digest, authority())
        .unwrap();
    let page = engine
        .inspect_catalog(&CatalogQuery::first(CatalogKind::Definitions, 25))
        .unwrap();
    let wire = serde_json::to_string(&page).unwrap();
    assert!(
        !wire.contains("PRIVATE_CAPTURED_SENTINEL")
            && !wire.contains("registry")
            && !wire.contains("source_map")
    );
    let CatalogRecord::Definition(definition) = &page.items[0].record else {
        panic!("definition")
    };
    assert_eq!(definition.artifact, value.digest);
    assert_eq!(definition.purpose.len(), 4096);
    assert!(definition.purpose_truncated);
    engine
        .set_activation_controls(
            "policy",
            ActivationControls {
                calls_enabled: false,
                event_starts_enabled: false,
                execution_enabled: false,
            },
            1,
        )
        .unwrap();
    let mut query = CatalogQuery::first(CatalogKind::Activations, 25);
    query.exact = Some(activation_key("policy"));
    let active = engine.inspect_catalog(&query).unwrap();
    let CatalogRecord::Activation(binding) = &active.items[0].record else {
        panic!("activation")
    };
    assert_eq!(binding.binding, "policy");
    assert!(!binding.controls.execution_enabled);
    assert!(binding.artifact_execution_denied);
}

#[test]
fn unrelated_state_progress_preserves_stamp_but_catalog_changes_refuse_old_pages() {
    let storage = MemoryStorage::default();
    let engine = Engine::new(storage.clone(), "catalog", "owner").unwrap();
    for name in ["one", "two"] {
        let value = artifact(name, "public");
        engine
            .install(Artifact::seal(value.content).unwrap())
            .unwrap();
    }
    let first = engine
        .inspect_catalog(&CatalogQuery::first(CatalogKind::Definitions, 1))
        .unwrap();
    let mut next = CatalogQuery::first(CatalogKind::Definitions, 1);
    next.after = first.next_cursor.clone();
    next.expected = Some(first.stamp.clone());
    let mut state = engine.snapshot().unwrap();
    state.sequence += 1;
    storage
        .compare_and_swap("catalog", "owner", first.state_revision, &state)
        .unwrap();
    let second = engine.inspect_catalog(&next).unwrap();
    assert_eq!(second.stamp, first.stamp);
    assert!(second.state_revision > first.state_revision);
    assert_ne!(second.items[0].key, first.items[0].key);
    let third = artifact("three", "public");
    engine
        .install(Artifact::seal(third.content).unwrap())
        .unwrap();
    assert!(matches!(
        engine.inspect_catalog(&next),
        Err(CatalogError::Conflict)
    ));
}

#[test]
fn byte_limited_pages_preserve_every_item_and_never_download_unbounded_metadata() {
    let engine = Engine::new(MemoryStorage::default(), "catalog", "owner").unwrap();
    for index in 0..26 {
        let value = artifact(&format!("policy-{index}"), &"\0".repeat(4096));
        engine
            .install(Artifact::seal(value.content).unwrap())
            .unwrap();
    }
    let mut query = CatalogQuery::first(CatalogKind::Definitions, 25);
    let mut keys = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = engine.inspect_catalog(&query).unwrap();
        pages += 1;
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_CATALOG_BYTES);
        assert!(page.items.len() < 25);
        for item in &page.items {
            assert!(keys.insert(item.key.clone()));
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        query.after = Some(cursor);
        query.expected = Some(page.stamp);
        assert!(pages <= 4);
    }
    assert_eq!(keys.len(), 26);
    assert!(pages > 1);
}

#[test]
fn release_and_reacquire_even_same_owner_refuse_previous_incarnation() {
    let storage = MemoryStorage::default();
    assert!(matches!(
        storage.inspect_catalog(
            "catalog",
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 1)
        ),
        Err(CatalogError::Ownership)
    ));
    let engine = Engine::new(storage.clone(), "catalog", "owner").unwrap();
    let first = engine
        .inspect_catalog(&CatalogQuery::first(CatalogKind::Definitions, 1))
        .unwrap();
    assert!(matches!(
        storage.inspect_catalog(
            "catalog",
            "other",
            &CatalogQuery::first(CatalogKind::Definitions, 1)
        ),
        Err(CatalogError::Ownership)
    ));
    engine.release().unwrap();
    let next = Engine::new(storage, "catalog", "owner").unwrap();
    let mut query = CatalogQuery::first(CatalogKind::Definitions, 1);
    query.expected = Some(first.stamp);
    assert!(matches!(
        next.inspect_catalog(&query),
        Err(CatalogError::Conflict)
    ));
}

#[test]
fn unsupported_metadata_does_not_change_existing_state_mutation_semantics() {
    let storage = MemoryStorage::default();
    let engine = Engine::new(storage.clone(), "catalog", "owner").unwrap();
    let mut state = engine.snapshot().unwrap();
    state.bindings.insert(
        "b".repeat(513),
        Activation {
            artifact: format!("sha256:{}", "a".repeat(64)),
            authority: authority(),
            revision: 1,
            controls: Default::default(),
        },
    );
    storage
        .compare_and_swap("catalog", "owner", 1, &state)
        .unwrap();
    assert!(matches!(
        engine.inspect_catalog(&CatalogQuery::first(CatalogKind::Activations, 25)),
        Err(CatalogError::Unsupported)
    ));
    assert_eq!(engine.snapshot().unwrap().bindings.len(), 1);
}

#[test]
fn wrappers_without_catalog_support_never_fall_back_to_loading_secrets() {
    struct Opaque;
    impl Storage for Opaque {
        fn load(&self, _: &str) -> std::result::Result<storage::VersionedState, StorageError> {
            panic!("full snapshot load")
        }
        fn acquire(&self, _: &str, _: &str) -> std::result::Result<(), StorageError> {
            Ok(())
        }
        fn release(&self, _: &str, _: &str) -> std::result::Result<(), StorageError> {
            Ok(())
        }
        fn compare_and_swap(
            &self,
            _: &str,
            _: &str,
            _: u64,
            _: &State,
        ) -> std::result::Result<u64, StorageError> {
            panic!("mutation")
        }
    }
    assert!(matches!(
        Opaque.inspect_catalog(
            "catalog",
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 1)
        ),
        Err(CatalogError::Unsupported)
    ));
}

#[test]
fn decimal_contexts_are_lossless_and_malformed_queries_are_closed() {
    let stamp = CatalogStamp {
        namespace: "catalog".into(),
        kind: CatalogKind::Definitions,
        incarnation: 9007199254740993,
        metadata_revision: 9007199254740994,
    };
    let wire = serde_json::to_value(&stamp).unwrap();
    assert_eq!(wire["incarnation"], "9007199254740993");
    assert_eq!(
        serde_json::from_value::<CatalogStamp>(wire.clone()).unwrap(),
        stamp
    );
    let mut invalid = wire;
    invalid["incarnation"] = json!(9007199254740993_u64);
    assert!(serde_json::from_value::<CatalogStamp>(invalid).is_err());
    let mut query = CatalogQuery::first(CatalogKind::Definitions, 26);
    assert!(query.validate().is_err());
    query.limit = 1;
    query.after = Some(format!("sha256:{}", "a".repeat(64)));
    assert!(query.validate().is_err());
    query.expected = Some(stamp);
    assert!(query.validate().is_ok());
    let mut value = serde_json::to_value(query).unwrap();
    value["private_query"] = json!(true);
    assert!(serde_json::from_value::<CatalogQuery>(value).is_err());
}
