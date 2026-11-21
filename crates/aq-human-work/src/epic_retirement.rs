//! Plan a registration and protect the existing draft tasks in one Graph write.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const KIND: &str = "epic.retirement.register";
const META: &str = "aq:epic_feature_retirement";
pub mod dispatch;
pub mod settle;

/// Require both draft identities to have settled under this owner's ordinary
/// task creation journal. Graph metadata by itself is not an AQ principal.
pub async fn owner_created(
    pool: &sqlx::PgPool,
    actor: Uuid,
    workspace: Uuid,
    input: &Register,
) -> Result<bool, sqlx::Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM human_graph_operations WHERE task_id = ANY($1)
         AND actor_id = $2 AND project_id = $3 AND workspace_id = $4 AND status->>'state' = 'confirmed'",
    )
    .bind(&[input.adopt.approval, input.adopt.cleanup][..])
    .bind(actor)
    .bind(input.project_id)
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    Ok(count == 2)
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Adopt {
    pub approval: Uuid,
    pub cleanup: Uuid,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub epic_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(range(min = 1, max = 2147483647))]
    pub generation: i32,
    pub repositories: BTreeMap<String, String>,
    pub ordinary_default: bool,
    #[schemars(length(min = 1, max = 4096))]
    pub permanent_behavior: String,
    pub adopt: Adopt,
}

impl Register {
    pub fn valid(&self) -> bool {
        let ids = [
            self.project_id,
            self.epic_id,
            self.adopt.approval,
            self.adopt.cleanup,
        ];
        !self.operation_id.is_nil()
            && self.generation > 0
            && !self.ordinary_default
            && ids
                .iter()
                .enumerate()
                .all(|(i, id)| !id.is_nil() && !ids[..i].contains(id))
            && (1..=32).contains(&self.repositories.len())
            && self.repositories.iter().all(|(repo, flag)| {
                repo.len() <= 96
                    && repo.split_once('/').is_some_and(|(owner, name)| {
                        [owner, name].iter().all(|part| {
                            !matches!(*part, "" | "." | "..")
                                && part
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                        })
                    })
                    && (1..=128).contains(&flag.len())
                    && flag
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            })
            && (1..=4096).contains(&self.permanent_behavior.len())
            && !self.permanent_behavior.trim().is_empty()
            && !self.permanent_behavior.contains('\0')
    }

    pub fn id(&self, workspace: Uuid) -> Uuid {
        Uuid::new_v5(
            &workspace,
            format!(
                "aq/epic-retirement/1/{}/{}/{}",
                self.project_id, self.epic_id, self.generation
            )
            .as_bytes(),
        )
    }

    pub fn plan(
        &self,
        workspace: Uuid,
        actor: Uuid,
        service: &str,
        epic: &Value,
        approval: &Value,
        cleanup: &Value,
    ) -> Option<Vec<Value>> {
        let registration = self.id(workspace);
        let closure = Uuid::new_v5(&registration, b"closure-gate");
        if epic["id"] != json!(self.epic_id)
            || epic["metadata"]["aq:project_id"] != json!(self.project_id)
            || !matches!(epic["task_state"].as_str(), Some("draft" | "active"))
        {
            return None;
        }
        let approval_edges = approval["edges"].as_array()?;
        let cleanup_edges = cleanup["edges"].as_array()?;
        let blocker = |edge: &Value| {
            edge["edge_type"] == "blocks"
                && edge["source"] == json!(self.adopt.approval)
                && edge["target"] == json!(self.adopt.cleanup)
        };
        if !approval_edges.iter().any(blocker)
            || !cleanup_edges.iter().any(blocker)
            || approval_edges.iter().any(|edge| {
                matches!(edge["edge_type"].as_str(), Some("blocks" | "parent-child"))
                    && !blocker(edge)
            })
        {
            return None;
        }
        let mut has_parent = false;
        for edge in cleanup_edges
            .iter()
            .filter(|edge| edge["target"] == json!(self.adopt.cleanup))
        {
            match edge["edge_type"].as_str() {
                Some("parent-child") if edge["source"] == json!(self.epic_id) => has_parent = true,
                Some("parent-child") => return None,
                Some("blocks") if !blocker(edge) => return None,
                _ => (),
            }
        }
        let draft = |context: &Value, id: Uuid| {
            let node = &context["node"];
            let metadata = node["metadata"].as_object()?;
            if context["truncated"] != false
                || node["id"] != json!(id)
                || node["task_state"] != "draft"
                || node["verification"] != "unverified"
                || node["created_by"] != service
                || metadata.get("aq:human_actor_id") != Some(&json!(actor))
                || metadata.get("aq:project_id") != Some(&json!(self.project_id))
                || metadata.contains_key(META)
            {
                return None;
            }
            Some((node["version"].as_u64()?, metadata.clone()))
        };
        let (approval_version, _) = draft(approval, self.adopt.approval)?;
        let (cleanup_version, mut cleanup_meta) = draft(cleanup, self.adopt.cleanup)?;
        cleanup_meta.insert(
            META.into(),
            json!({"registration":registration,"role":"cleanup"}),
        );
        let child = |id: Uuid| json!({"op":"add_edge","id":Uuid::new_v5(&registration, format!("parent-child/{}/{id}", self.epic_id).as_bytes()),"edge_type":"parent-child","source":self.epic_id,"target":id});
        let verify = |id: Uuid, version: u64| json!({"op":"verify_node","id":id,"expected_version":version,"verification":"verified","evidence":[{"kind":"task","source_id":"epic","task_id":self.epic_id}]});
        let state = json!({"spec":{"epic_id":self.epic_id,"project_id":self.project_id,"generation":self.generation,
            "approval":self.adopt.approval,"cleanup":self.adopt.cleanup,"closure_gate":closure,"adopt":true,
            "owner_authority":{"kind":"aq_owner","principal_id":actor},"ordinary_default":false,
            "permanent_behavior":self.permanent_behavior,"repositories":self.repositories},
            "repositories":self.repositories,"merges":{},"ordinary":{},"setup":registration});
        let mut operations = vec![
            json!({"op":"update_node","id":self.epic_id,"expected_version":epic["version"],"patch":{}}),
            json!({"op":"update_node","id":self.adopt.approval,"expected_version":approval_version,"patch":{}}),
            json!({"op":"update_node","id":self.adopt.cleanup,"expected_version":cleanup_version,"patch":{"metadata":cleanup_meta}}),
            json!({"op":"create_node","id":closure,"kind":"task","task_state":"draft","title":"Verify epic retirement evidence",
                "metadata":{"aq:project_id":self.project_id,"aq:human_actor_id":actor,(META):{"registration":registration,"role":"closure_gate"}}}),
            child(closure),
        ];
        if !has_parent {
            operations.push(child(self.adopt.cleanup));
        }
        operations.extend([
            verify(self.adopt.cleanup, cleanup_version + 1 + u64::from(!has_parent)),
            verify(closure, 2),
            json!({"op":"create_node","id":registration,"kind":"knowledge","content_type":"policy","title":"Register epic feature flags",
                "metadata":{"aq:project_id":self.project_id,"aq:human_actor_id":actor,"aq:human_operation_id":self.operation_id,(META):state}}),
            verify(registration, 1),
        ]);
        Some(operations)
    }
}
