//! The model-independent graph access policy of a launch. A resolved API01
//! profile is derived only after a concrete intelligence choice exists.
mod context;
use crate::*;
use aq_session_api::Permission;
use aq_session_protocol::{ArtifactRef, Text};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAccessPolicyKind {
    PreselectionGraphAccessPolicyV1,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphEdgeType {
    Blocks,
    #[serde(rename = "parent-child")]
    ParentChild,
    References,
    MotivatedBy,
    Produces,
    Supports,
    Contradicts,
    Supersedes,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphAccessScope {
    pub repository_id: Uuid,
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub record_ids: Vec<Uuid>,
    pub edge_types: Vec<GraphEdgeType>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphAccessPolicy {
    pub schema_version: u32,
    pub kind: GraphAccessPolicyKind,
    pub project_binding_id: Uuid,
    pub project_revision: u64,
    pub source_context_revision: String,
    pub context_digest: String,
    pub instructions_digest: String,
    pub tool_policy_digest: String,
    pub effective_permissions: Vec<Permission>,
    pub scope: GraphAccessScope,
}
impl GraphAccessPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure(
            self.schema_version == 1
                && !self.project_binding_id.is_nil()
                && self.project_revision > 0,
            "graph policy identity",
        )?;
        text(&self.source_context_revision)?;
        for value in [
            &self.context_digest,
            &self.instructions_digest,
            &self.tool_policy_digest,
        ] {
            ensure(valid_digest(value), "graph policy digest")?;
        }
        self.scope.validate()?;
        ensure(
            self.effective_permissions.len() <= 128,
            "graph policy permission bound",
        )?;
        let mut permissions = BTreeSet::new();
        for permission in &self.effective_permissions {
            text(&permission.scope_reference)?;
            let expected = if permission.operation == "source.write" {
                format!("repository:{}", self.scope.repository_id)
            } else {
                format!("project:{}", self.project_binding_id)
            };
            ensure(
                permission.scope_reference == expected,
                "graph policy permission scope does not name owned project/repository",
            )?;
            ensure(
                ["task.read", "knowledge.read", "source.write"]
                    .contains(&permission.operation.as_str())
                    && permissions.insert((&permission.operation, &permission.scope_reference)),
                "unsupported graph policy permission",
            )?;
        }
        Ok(())
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        let value = digest(self)?;
        Ok(ArtifactRef {
            id: Text::new(format!("aq.preselection-graph-access-policy/{value}"))
                .map_err(|_| Error::Invalid("graph policy artifact identity".into()))?,
            version: Text::new("1").unwrap(),
            digest: serde_json::from_value(serde_json::json!(value))
                .map_err(|_| Error::Invalid("graph policy artifact digest".into()))?,
        })
    }
}
impl GraphAccessScope {
    fn validate(&self) -> Result<()> {
        ensure(
            [self.repository_id, self.workspace_id, self.task_id]
                .iter()
                .all(|id| !id.is_nil())
                && (1..=256).contains(&self.record_ids.len())
                && self.record_ids.contains(&self.task_id)
                && self.record_ids.iter().all(|id| !id.is_nil())
                && self.record_ids.iter().collect::<BTreeSet<_>>().len() == self.record_ids.len(),
            "graph policy record scope",
        )?;
        ensure(
            self.edge_types.len() <= 8
                && self
                    .edge_types
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .collect::<BTreeSet<_>>()
                    .len()
                    == self.edge_types.len(),
            "graph policy edge scope",
        )?;
        Ok(())
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// Supplied only after an actual concrete intelligence choice exists. These
/// values are not availability, authentication or execution authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedGraphModel {
    pub provider: String,
    pub model: String,
    pub native_reasoning: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphRetryBudget {
    pub max_attempts: u8,
    pub max_elapsed_ms: u64,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}
impl GraphAccessPolicy {
    pub fn resolve_profile(
        &self,
        model: &ResolvedGraphModel,
        context: &serde_json::Value,
        role: &str,
        retry: &GraphRetryBudget,
    ) -> Result<serde_json::Value> {
        self.validate_context(context)?;
        for value in [&model.provider, &model.model, &model.native_reasoning] {
            text(value)?;
        }
        let readable = self
            .effective_permissions
            .iter()
            .any(|p| ["task.read", "knowledge.read"].contains(&p.operation.as_str()));
        let capabilities = if readable {
            vec![
                "read.get",
                "read.revision",
                "read.history",
                "read.search",
                "read.context",
                "read.relationships",
            ]
        } else {
            vec![]
        };
        let policy_digest = digest(self)?;
        let profile = serde_json::json!({
            "profile_id":format!("native/{}",digest(&(&policy_digest,model,role,retry))?),"revision":self.project_revision.to_string(),"role":role,
            "model":{"provider":model.provider,"model":model.model,"configuration_digest":digest(model)?},
            "instructions_digest":self.instructions_digest,"tool_policy_digest":self.tool_policy_digest,"capabilities":capabilities,
            "read_scope":{"mode":"records","record_ids":self.scope.record_ids,"include_created_by_attempt":false,"edge_types":self.scope.edge_types},
            "write_scope":{"record_ids":[],"relationship_types":[],"correct_fields":[],"max_created_records":0,"max_proposal_tasks":0},
            "retry_budget":retry,"max_response_bytes":1_048_576,
        });
        ensure(
            aq_agent_api::valid("ResolvedProfile", &profile),
            "resolved API01 profile bounds",
        )?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> serde_json::Value {
        serde_json::json!({"observed_at":"1970-01-01T00:00:01Z","projection":"authorized_scope","root_ids":[Uuid::from_u128(3)],"nodes":[{"id":Uuid::from_u128(3),"kind":"task","version":"1","content_revision":"1","title":"Synthetic task","body":"Bounded work","acceptance_criteria":"Verify","labels":[],"task_state":"active","verification":null,"content_type":null,"retired":false,"deleted":false}],"edges":[],"pinned_revisions":[],"coverage":{"status":"complete","complete_for":"authorized_projection","reasons":[]}})
    }
    fn policy(context: &serde_json::Value, records: Vec<Uuid>) -> GraphAccessPolicy {
        let project = Uuid::from_u128(1);
        let permission = |operation: &str, scope: String| Permission {
            operation: operation.into(),
            scope_reference: scope,
        };
        GraphAccessPolicy {
            schema_version: 1,
            kind: GraphAccessPolicyKind::PreselectionGraphAccessPolicyV1,
            project_binding_id: project,
            project_revision: 1,
            source_context_revision: "task:1".into(),
            context_digest: digest(context).unwrap(),
            instructions_digest: "a".repeat(64),
            tool_policy_digest: "b".repeat(64),
            effective_permissions: vec![
                permission("task.read", format!("project:{project}")),
                permission("knowledge.read", format!("project:{project}")),
                permission(
                    "source.write",
                    format!("repository:{}", Uuid::from_u128(41)),
                ),
            ],
            scope: GraphAccessScope {
                repository_id: Uuid::from_u128(41),
                workspace_id: Uuid::from_u128(2),
                task_id: Uuid::from_u128(3),
                record_ids: records,
                edge_types: vec![],
            },
        }
    }
    fn model(name: &str) -> ResolvedGraphModel {
        ResolvedGraphModel {
            provider: "fixture-a".into(),
            model: name.into(),
            native_reasoning: "high".into(),
        }
    }
    const RETRY: GraphRetryBudget = GraphRetryBudget {
        max_attempts: 3,
        max_elapsed_ms: 60000,
        initial_delay_ms: 100,
        max_delay_ms: 1000,
    };

    #[test]
    fn two_models_share_one_policy_and_resolve_distinct_profiles() {
        let context = context();
        let policy = policy(&context, vec![Uuid::from_u128(3)]);
        let resolve =
            |name| (policy.resolve_profile(&model(name), &context, "developer", &RETRY)).unwrap();
        let (first, second) = (resolve("model-a"), resolve("model-b"));
        assert_eq!(first["model"]["model"], "model-a");
        assert_ne!(first["profile_id"], second["profile_id"]);
        assert_eq!(first["instructions_digest"], second["instructions_digest"]);
        assert_eq!(first["tool_policy_digest"], second["tool_policy_digest"]);
        assert_eq!(
            first["read_scope"]["record_ids"],
            serde_json::json!(policy.scope.record_ids)
        );
        // The policy holds for exactly the context it was sealed for.
        let mut changed = context.clone();
        changed["nodes"][0]["body"] = serde_json::json!("changed");
        assert!(policy.validate_context(&changed).is_err());
    }

    #[test]
    fn a_context_record_needs_its_typed_read_permission_at_the_owned_scope() {
        let mut context = context();
        let mut knowledge = context["nodes"][0].clone();
        knowledge["id"] = serde_json::json!(Uuid::from_u128(9));
        knowledge["kind"] = serde_json::json!("knowledge");
        knowledge["task_state"] = serde_json::Value::Null;
        context["nodes"].as_array_mut().unwrap().push(knowledge);
        let sealed = policy(&context, vec![Uuid::from_u128(3), Uuid::from_u128(9)]);
        sealed.validate_context(&context).unwrap();
        for missing in ["task.read", "knowledge.read"] {
            let mut denied = sealed.clone();
            (denied.effective_permissions).retain(|p| p.operation != missing);
            assert!(denied.validate_context(&context).is_err(), "{missing}");
        }
        let mut wrong = sealed.clone();
        wrong.effective_permissions[0].scope_reference =
            format!("project:{}", Uuid::from_u128(8888));
        assert!(wrong.validate_context(&context).is_err());
        let mut alias = sealed.clone();
        alias.effective_permissions[0].scope_reference = "project:example".into();
        assert!(alias.validate_context(&context).is_err());
        // A record outside the sealed scope, or a scope the context does not cover.
        let narrower = policy(&context, vec![Uuid::from_u128(3)]);
        assert!(narrower.validate_context(&context).is_err());
    }
}
