//! The permissions of one attempt, derived from its role (spec 0014 §6.2).
use super::Role;
use aq_session_api::Permission;
use uuid::Uuid;

fn holds(list: &[Permission], p: &Permission) -> bool {
    list.iter()
        .any(|q| q.operation == p.operation && q.scope_reference == p.scope_reference)
}
/// Each operation the role lists, scoped to the task's project
/// (`project:<id>`), for `source.*` to the project's repository
/// (`repository:<id>`), or for `wait.*` to the attempt itself
/// (`attempt:self`), kept only when both the project's `ceilings` and the
/// `caller`'s authority hold exactly that permission. The role's order is
/// kept and a repeated operation appears once.
pub fn effective_permissions(
    role: &Role,
    project_id: Uuid,
    repository_id: Uuid,
    ceilings: &[Permission],
    caller: &[Permission],
) -> Vec<Permission> {
    let mut out = Vec::new();
    for operation in &role.operations {
        let scope_reference = if operation.starts_with("source.") {
            format!("repository:{repository_id}")
        } else if operation.starts_with("wait.") {
            "attempt:self".to_string()
        } else {
            format!("project:{project_id}")
        };
        let permission = Permission {
            operation: operation.clone(),
            scope_reference,
        };
        if holds(ceilings, &permission) && holds(caller, &permission) && !holds(&out, &permission) {
            out.push(permission);
        }
    }
    out
}
