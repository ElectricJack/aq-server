//! The permissions of one attempt, derived from its role (spec 0014 §6.2).
use super::Role;
use aq_session_api::Permission;
use uuid::Uuid;

fn holds(list: &[Permission], p: &Permission) -> bool {
    list.iter()
        .any(|q| q.operation == p.operation && q.scope_reference == p.scope_reference)
}
/// The scope-qualified operations requested by a role.
pub struct RequestedPermissions(Vec<Permission>);

impl RequestedPermissions {
    pub fn into_permissions(self) -> Vec<Permission> {
        self.0
    }

    /// Preserve role order while enforcing both permission ceilings.
    pub fn effective(&self, ceilings: &[Permission], caller: &[Permission]) -> Vec<Permission> {
        self.0
            .iter()
            .filter(|permission| holds(ceilings, permission) && holds(caller, permission))
            .cloned()
            .collect()
    }
}
/// What the role asks for before any ceiling: each operation it lists, once,
/// in the role's order, scoped to its project, repository or attempt.
pub fn requested_permissions(
    role: &Role,
    project_id: Uuid,
    repository_id: Uuid,
) -> RequestedPermissions {
    let mut out: Vec<Permission> = Vec::new();
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
        if !holds(&out, &permission) {
            out.push(permission);
        }
    }
    RequestedPermissions(out)
}
