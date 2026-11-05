use aq_admission::{
    Digest,
    execution_spec::ProfiledExecutionSpec,
    outcome::{self, ExpectedOutcomeDefinition},
};
fn fixture() -> ExpectedOutcomeDefinition {
    outcome::decode(include_bytes!("../artifacts/outcome.fixture.json")).unwrap()
}
fn spec(outcome: &ExpectedOutcomeDefinition) -> ProfiledExecutionSpec {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/execution-spec.fixture.json")).unwrap();
    let mut spec: ProfiledExecutionSpec = serde_json::from_value(raw["spec"].clone()).unwrap();
    spec.outcome_artifact = outcome.artifact().unwrap();
    spec
}
#[test]
fn fixture_matches_independent_python_canonical_vectors_and_public_schema() {
    let outcome = fixture();
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/outcome.python-vectors.json")).unwrap();
    assert_eq!(
        outcome.artifact().unwrap().digest.as_str(),
        vectors["canonical_sha256"]
    );
    assert_eq!(
        outcome.canonical_bytes().unwrap().len(),
        vectors["canonical_bytes"].as_u64().unwrap() as usize
    );
    assert_eq!(
        outcome::schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/outcome.schema.json"))
            .unwrap()
    );
    outcome.validate_against(&spec(&outcome)).unwrap();
    let roundtrip = outcome::decode(&outcome.canonical_bytes().unwrap()).unwrap();
    assert_eq!(roundtrip, outcome);
}
#[test]
fn resealing_does_not_allow_different_task_source_or_context() {
    let original = fixture();
    for field in [
        "project",
        "project_revision",
        "task_version",
        "content_revision",
        "repository",
        "binding",
        "base",
        "source_artifact",
        "observation",
        "context",
    ] {
        let mut changed = original.clone();
        match field {
            "project" => changed.project_id = uuid::Uuid::from_u128(12345),
            "project_revision" => changed.project_revision += 1,
            "task_version" => changed.task.version += 1,
            "content_revision" => changed.task.content_revision += 1,
            "repository" => changed.source.repository_id = uuid::Uuid::from_u128(12345),
            "binding" => changed.source.binding_generation += 1,
            "base" => changed.source.base.hex = "1".repeat(changed.source.base.hex.len()),
            "source_artifact" => changed.source.artifact.digest = Digest::of(b"changed"),
            "observation" => changed.task_observation.digest = Digest::of(b"changed"),
            "context" => changed.task_context.digest = Digest::of(b"changed"),
            _ => unreachable!(),
        }
        changed.validate().unwrap();
        let mut s = spec(&original);
        s.outcome_artifact = changed.artifact().unwrap();
        assert!(changed.validate_against(&s).is_err(), "{field}");
    }
}
#[test]
fn unknown_policy_shapes_and_approval_shortcuts_cannot_decode() {
    let raw = serde_json::to_value(fixture()).unwrap();
    for (field, value) in [
        ("schema_version", serde_json::json!(2)),
        ("kind", serde_json::json!("agent_success")),
        ("review", serde_json::json!("none")),
        ("allow_auto_complete", serde_json::json!(true)),
    ] {
        let mut changed = raw.clone();
        changed[field] = value;
        assert!(
            outcome::decode(&serde_json::to_vec(&changed).unwrap()).is_err(),
            "{field}"
        );
    }
}
#[test]
fn requirements_have_unique_ids_and_preserve_nonempty_bounded_text() {
    let mut value = fixture();
    value
        .acceptance_criteria
        .push(value.acceptance_criteria[0].clone());
    assert!(value.validate().is_err());
    let mut value = fixture();
    value.required_checks.push(value.required_checks[0].clone());
    assert!(value.validate().is_err());
    let mut value = fixture();
    let mut input = value.inputs[0].clone();
    input.digest = Digest::of(b"different same logical reference");
    value.inputs.push(input);
    assert!(value.validate().is_err());
    for text in [" ".to_string(), "\0".to_string(), "雪".repeat(2731)] {
        let mut value = fixture();
        value.acceptance_criteria[0].text = text;
        assert!(value.validate().is_err());
    }
    let mut value = fixture();
    value.acceptance_criteria.clear();
    assert!(value.validate().is_err());
}
#[test]
fn byte_limit_covers_canonical_definition_and_raw_input() {
    let mut value = fixture();
    let criterion = value.acceptance_criteria[0].clone();
    value.acceptance_criteria.clear();
    for i in 0..9 {
        let mut c = criterion.clone();
        c.id = aq_admission::Label::new(format!("criterion-{i}")).unwrap();
        c.text = "x".repeat(8192);
        value.acceptance_criteria.push(c);
    }
    assert!(value.validate().is_err());
    let mut raw = fixture().canonical_bytes().unwrap();
    raw.resize(outcome::MAX_BYTES + 1, b' ');
    assert!(outcome::decode(&raw).is_err());
}
#[test]
fn changed_review_requirements_change_the_pinned_definition() {
    let original = fixture();
    let mut changed = original.clone();
    changed.review = outcome::ReviewRequirement::PolicyActor;
    assert!(changed.validate_against(&spec(&original)).is_err());
    assert_ne!(changed.artifact().unwrap(), original.artifact().unwrap());
    changed.validate_against(&spec(&changed)).unwrap();
    // This only proves specification linkage; no review/check execution or release
    // result exists in this pure definition API.
}
