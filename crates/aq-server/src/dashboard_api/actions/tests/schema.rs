use super::*;
#[test]
fn canonical_request_identity_normalizes_keys_but_binds_scope_and_values() {
    let a:ActionRequest=serde_json::from_value(json!({"operation_id":"11111111-1111-4111-8111-111111111111","kind":"test.effect","subject":{"type":"test","id":"one"},"expected_versions":{"b":"2","a":"1"},"payload":{"z":1,"a":2}})).unwrap();
    let b:ActionRequest=serde_json::from_str(r#"{"payload":{"a":2,"z":1},"expected_versions":{"a":"1","b":"2"},"subject":{"id":"one","type":"test"},"kind":"test.effect","operation_id":"11111111-1111-4111-8111-111111111111"}"#).unwrap();
    let digest = canonical_digest(&a, &Scope::AllAuthorized).unwrap();
    assert_eq!(digest, canonical_digest(&b, &Scope::AllAuthorized).unwrap());
    assert_ne!(
        digest,
        canonical_digest(
            &b,
            &Scope::Project {
                project_id: Uuid::new_v4()
            }
        )
        .unwrap()
    );
    let mut changed = b;
    changed.reason = Some("changed".into());
    assert_ne!(
        digest,
        canonical_digest(&changed, &Scope::AllAuthorized).unwrap()
    );
    assert!(digest.starts_with("sha256:"));
    assert_eq!(digest.len(), 71);
}

#[test]
fn registry_is_finite_versioned_and_rejects_open_payload_schemas() {
    #[derive(Deserialize, JsonSchema)]
    struct Open {
        _value: String,
    }
    struct UnsafeAction;
    #[async_trait]
    impl ActionKind for UnsafeAction {
        type Payload = Open;
        const KIND: &'static str = "unsafe.run";
        const GRANT: &'static str = "unsafe.run";
        const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
        async fn authorize(
            &self,
            _: &AppState,
            _: &User,
            _: &ActionRequest,
            _: &Open,
        ) -> Result<Scope, ApiError> {
            Ok(Scope::AllAuthorized)
        }
        async fn authorize_lookup(
            &self,
            _: &AppState,
            _: &User,
            _: &Scope,
            _: Option<&Operation>,
        ) -> Result<(), ApiError> {
            Ok(())
        }
        async fn execute(&self, _: &AppState, _: &User, _: &ActionRequest, _: Open) -> Outcome {
            Outcome::unknown()
        }
    }
    let mut registry = Registry::default();
    assert!(registry.register(UnsafeAction).is_err());
    assert!(registry.schemas().is_empty());
    registry
        .register(TestAction(Arc::new(AtomicUsize::new(0))))
        .unwrap();
    assert!(
        registry
            .register(TestAction(Arc::new(AtomicUsize::new(0))))
            .is_err()
    );
    assert!(contract::schema(&Registry::default())["$defs"]["ActionRequest"]["not"].is_object());
    assert_eq!(
        contract::schema(&registry)["$defs"]["ActionRequest"]["oneOf"][0]["properties"]["kind"]["const"],
        "test.effect"
    );
}

#[test]
fn dashboard_action_schema_lists_task_and_knowledge_kinds() {
    let registry = existing::schemas();
    for kind in [
        "task.graph.create",
        "task.edit",
        "task.activate",
        "task.reopen",
        "task.complete",
        "task.cancel",
        "task.retry",
        "task.reprioritize",
        "task.role.set",
        "task.route.set",
        "task.route.clear",
        "task.dependency.add",
        "task.dependency.remove",
        "knowledge.author",
        "knowledge.correct",
        "knowledge.verify",
        "knowledge.retire",
        "knowledge.restore",
        "playbook.run.start",
        "playbook.run.cancel",
        "playbook.activation.activate",
        "playbook.activation.rollback",
    ] {
        let schema = registry
            .schemas()
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} schema"));
        assert_eq!(schema["type"], "object", "{kind}");
        assert_eq!(schema["additionalProperties"], false, "{kind}");
        for field in ["operation_id", "project_id"] {
            assert!(schema["properties"][field].is_object(), "{kind} {field}");
        }
    }
}
