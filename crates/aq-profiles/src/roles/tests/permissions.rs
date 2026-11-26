use super::*;

fn permission(operation: &str, scope: String) -> Permission {
    Permission {
        operation: operation.into(),
        scope_reference: scope,
    }
}
fn pairs(list: &[Permission]) -> Vec<(String, String)> {
    list.iter()
        .map(|p| (p.operation.clone(), p.scope_reference.clone()))
        .collect()
}
#[test]
fn permissions_take_the_scope_of_the_project_or_its_repository() {
    let project = Uuid::from_u128(7);
    let repository = Uuid::from_u128(9);
    // What compose.rs builds today as the installation ceiling for the live case.
    let mut live = ["task.read", "knowledge.read"]
        .map(|operation| permission(operation, format!("project:{project}")))
        .to_vec();
    live.push(permission(
        "source.write",
        format!("repository:{repository}"),
    ));
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let developer = requested_permissions(snapshot.role("developer").unwrap(), project, repository);
    let effective = developer.effective(&live, &live);
    assert_eq!(pairs(&effective), pairs(&live));

    // A reviewer does not write source, whatever the ceilings allow.
    let reviewer = requested_permissions(snapshot.role("reviewer").unwrap(), project, repository);
    assert_eq!(pairs(&reviewer.effective(&live, &live)), pairs(&live[..2]));
    // Both the project's ceilings and the caller must hold the exact permission.
    assert_eq!(
        pairs(&developer.effective(&live[1..], &live)),
        pairs(&live[1..])
    );
    assert_eq!(
        pairs(&developer.effective(&live, &live[..1])),
        pairs(&live[..1])
    );
    assert!(developer.effective(&live, &[]).is_empty());
    // A ceiling for another project or with the wrong scope kind grants nothing.
    let foreign = vec![
        permission("task.read", format!("project:{}", Uuid::from_u128(8))),
        permission("source.write", format!("project:{project}")),
        permission("knowledge.read", format!("repository:{repository}")),
    ];
    assert!(developer.effective(&foreign, &live).is_empty());
}
#[test]
fn wait_operations_require_attempt_scoped_permissions() {
    let project = Uuid::from_u128(7);
    let repository = Uuid::from_u128(9);
    let snapshot = compile(&default_documents(), &defaults(), &limits()).unwrap();
    let developer = snapshot.role("developer").unwrap();
    let mut waiter = developer.clone();
    waiter.operations = vec![
        "wait.register".into(),
        "task.read".into(),
        "wait.register".into(),
        "wait.show".into(),
    ];
    let requested = requested_permissions(&waiter, project, repository);
    let ceiling = vec![
        permission("task.read", format!("project:{project}")),
        permission("wait.register", "attempt:self".into()),
        // A project-scoped wait is not what a role's wait operation means.
        permission("wait.show", format!("project:{project}")),
    ];
    assert_eq!(
        pairs(&requested.effective(&ceiling, &ceiling)),
        [
            ("wait.register".to_string(), "attempt:self".to_string()),
            ("task.read".to_string(), format!("project:{project}")),
        ]
    );
}
#[test]
fn role_operations_are_the_known_vocabulary() {
    for operation in aq_agent_grants::op::ALL {
        assert!(operation_known(operation));
    }
    assert!(operation_known("source.write"));
    for operation in aq_agent_grants::OWNER_ONLY {
        assert!(!operation_known(operation));
    }
    assert!(!operation_known("source.read") && !operation_known(""));
}
