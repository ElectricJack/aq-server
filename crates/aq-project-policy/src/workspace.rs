//! Decide a sibling checkout against the *target* project's policy.
//! A pin describes the input to hydration. It is never a credential.
use crate::{ExternalStatus, ProjectRecord};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    Preauthorized,
    NeedsApproval,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePin {
    pub project_id: Uuid,
    pub project_revision: u64,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub commit: String,
    pub access: Access,
}

/// Each requirement has exactly one shape. Owner requests and creation use
/// the same representation as the attempt command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum Requirement {
    Source {
        target_project_id: Uuid,
        access: Access,
    },
    Service {
        service: Service,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Postgres,
}

pub fn valid_list(requirements: &[Requirement]) -> bool {
    let mut projects = std::collections::BTreeSet::new();
    let mut postgres = false;
    requirements.len() <= 17
        && requirements.iter().all(|req| match req {
            Requirement::Source {
                target_project_id, ..
            } => {
                !target_project_id.is_nil()
                    && projects.insert(*target_project_id)
                    && projects.len() <= 16
            }
            Requirement::Service {
                service: Service::Postgres,
            } => !std::mem::replace(&mut postgres, true),
        })
}

pub fn disposition(
    home: Uuid,
    target: &ProjectRecord,
    access: Access,
) -> Result<Disposition, &'static str> {
    let binding = &target.binding;
    if home.is_nil() || home == binding.project_id {
        return Err("a sibling checkout requires a different home project");
    }
    if !binding.enabled {
        return Err("workspace target project is disabled");
    }
    let scope = format!("project:{home}");
    let permits = |operation| {
        binding
            .ceilings
            .permissions
            .iter()
            .any(|rule| rule.scope_reference == scope && rule.operation == operation)
    };
    if permits("workspace.share.write")
        || (access == Access::Read && permits("workspace.share.read"))
    {
        return Ok(Disposition::Preauthorized);
    }
    if permits("workspace.share.request") {
        return Ok(Disposition::NeedsApproval);
    }
    Err("target project has not enabled this workspace request")
}

impl SourcePin {
    /// Refuse changed ownership or revision even when the repository's name
    /// and commit are unchanged. The source adapter separately verifies the OID.
    pub fn validate(&self, target: &ProjectRecord) -> Result<(), &'static str> {
        let binding = &target.binding;
        if !binding.enabled
            || self.project_id != binding.project_id
            || self.project_revision != target.revision
            || self.repository_id != binding.repository_id
            || self.binding_generation != binding.repository_binding_generation
            || target.validation.repository != ExternalStatus::Verified
            || target.validation.repository_id != self.repository_id
            || target.validation.repository_generation != Some(self.binding_generation)
        {
            return Err("workspace target binding changed");
        }
        if !aq_delivery::source_checks::hex40(&self.commit) {
            return Err("workspace source requires a full commit object ID");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BindingObservation, OperationScope};

    fn target() -> ProjectRecord {
        let binding = crate::tests::binding();
        ProjectRecord {
            revision: 3,
            validated_at_ms: 1,
            validation: BindingObservation {
                repository_id: binding.repository_id,
                graph_workspace_id: binding.graph_workspace_id,
                repository_adapter_reference: binding.repository_adapter_reference.clone(),
                graph_adapter_reference: binding.graph_adapter_reference.clone(),
                repository: ExternalStatus::Verified,
                graph: ExternalStatus::Verified,
                repository_generation: Some(binding.repository_binding_generation),
                observed_at_ms: 1,
                expires_at_ms: 10,
            },
            binding,
        }
    }

    #[test]
    fn the_target_controls_sharing_and_write_requires_its_own_grant() {
        let mut target = target();
        let home = Uuid::from_u128(100);
        let rule = |op: &str| OperationScope {
            operation: format!("workspace.share.{op}"),
            scope_reference: format!("project:{home}"),
        };
        assert!(disposition(home, &target, Access::Read).is_err());
        target
            .binding
            .ceilings
            .permissions
            .extend([rule("request"), rule("read")]);
        assert_eq!(
            disposition(home, &target, Access::Read),
            Ok(Disposition::Preauthorized)
        );
        assert_eq!(
            disposition(home, &target, Access::Write),
            Ok(Disposition::NeedsApproval)
        );
        assert!(disposition(Uuid::from_u128(101), &target, Access::Read).is_err());
        target.binding.ceilings.permissions.push(rule("write"));
        assert_eq!(
            disposition(home, &target, Access::Write),
            Ok(Disposition::Preauthorized)
        );
        target.binding.enabled = false;
        assert!(disposition(home, &target, Access::Read).is_err());
    }

    #[test]
    fn pin_binds_project_revision_repository_generation_and_exact_oid() {
        let target = target();
        let mut pin = SourcePin {
            project_id: target.binding.project_id,
            project_revision: target.revision,
            repository_id: target.binding.repository_id,
            binding_generation: target.binding.repository_binding_generation,
            commit: "abcd".repeat(10),
            access: Access::Write,
        };
        assert_eq!(pin.validate(&target), Ok(()));
        pin.binding_generation += 1;
        assert!(pin.validate(&target).is_err());
        pin.binding_generation -= 1;
        pin.commit = "main".into();
        assert!(pin.validate(&target).is_err());
    }
}
