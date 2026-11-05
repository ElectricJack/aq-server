use aq_admission::transport::task_lineage_id;
use serde_json::Value;
use uuid::Uuid;

#[test]
fn independent_python_raw_uuid_vectors_are_stable() {
    let golden: Value =
        serde_json::from_str(include_str!("../artifacts/task-lineage-v1.json")).unwrap();
    for vector in golden["vectors"].as_array().unwrap() {
        let id = |field: &str| Uuid::parse_str(vector[field].as_str().unwrap()).unwrap();
        assert_eq!(
            task_lineage_id(id("project_id"), id("workspace_id"), id("task_id")).unwrap(),
            id("lineage_id")
        );
    }
}

#[test]
fn namespace_cannot_be_reset_by_attempt_and_nil_scope_refuses() {
    let project = Uuid::from_u128(1);
    let workspace = Uuid::from_u128(2);
    let task = Uuid::from_u128(3);
    let original = task_lineage_id(project, workspace, task).unwrap();
    assert_eq!(original, task_lineage_id(project, workspace, task).unwrap());
    assert_ne!(
        original,
        task_lineage_id(project, workspace, Uuid::from_u128(4)).unwrap()
    );
    assert!(task_lineage_id(Uuid::nil(), workspace, task).is_err());
    assert!(task_lineage_id(project, Uuid::nil(), task).is_err());
    assert!(task_lineage_id(project, workspace, Uuid::nil()).is_err());
}
