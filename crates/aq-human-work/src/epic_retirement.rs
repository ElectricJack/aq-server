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

/// Whether this owner's confirmed `epic.retirement.register` operation, the
/// one the record names, created `registration` in this workspace and project.
/// A Graph record by itself cannot show that it came through the AQ owner API.
pub async fn owner_registered(
    pool: &sqlx::PgPool,
    (actor, workspace, project): (Uuid, Uuid, Uuid),
    operation: Uuid,
    registration: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM dashboard_operations d JOIN dashboard_graph_intents i USING(operation_id)
         WHERE d.operation_id=$1 AND d.principal_id=$2 AND d.kind=$3 AND d.disposition='confirmed'
         AND i.workspace_id=$4 AND i.project_id=$5
         AND convert_from(i.body,'UTF8')::jsonb->'operations' @> $6::jsonb)",
    )
    .bind(operation)
    .bind(actor)
    .bind(KIND)
    .bind(workspace)
    .bind(project)
    .bind(json!([{"op":"create_node","id":registration,"kind":"knowledge"}]).to_string())
    .fetch_one(pool)
    .await
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

/// Authenticated owner, project binding and Graph mutation identity.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub workspace: Uuid,
    pub actor: Uuid,
    pub project: Uuid,
    pub service: &'a str,
}

struct AdoptedDraft {
    version: u64,
    metadata: serde_json::Map<String, Value>,
}

impl Adopt {
    fn blocker(&self, edge: &Value) -> bool {
        edge["edge_type"] == "blocks"
            && edge["source"] == json!(self.approval)
            && edge["target"] == json!(self.cleanup)
    }
    fn cleanup_has_parent(&self, epic: Uuid, approval: &Value, cleanup: &Value) -> Option<bool> {
        let approval_edges = approval["edges"].as_array()?;
        let cleanup_edges = cleanup["edges"].as_array()?;
        let has_blocker = |edges: &[Value]| edges.iter().any(|e| self.blocker(e));
        if !has_blocker(approval_edges) || !has_blocker(cleanup_edges) {
            return None;
        }
        if approval_edges.iter().any(|e| {
            matches!(e["edge_type"].as_str(), Some("blocks" | "parent-child")) && !self.blocker(e)
        }) {
            return None;
        }
        self.parent_edge(epic, cleanup_edges)
    }
    fn parent_edge(&self, epic: Uuid, cleanup_edges: &[Value]) -> Option<bool> {
        let mut has_parent = false;
        for edge in cleanup_edges
            .iter()
            .filter(|e| e["target"] == json!(self.cleanup))
        {
            match edge["edge_type"].as_str() {
                Some("parent-child") if edge["source"] == json!(epic) => has_parent = true,
                Some("parent-child") => return None,
                Some("blocks") if !self.blocker(edge) => return None,
                _ => (),
            }
        }
        Some(has_parent)
    }
}
fn part(s: &str) -> bool {
    !matches!(s, "" | "." | "..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn repository(s: &str) -> bool {
    s.len() <= 96
        && s.split_once('/')
            .is_some_and(|(owner, name)| part(owner) && part(name))
}
fn flag(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn is_draft(context: &Value, id: Uuid) -> bool {
    let node = &context["node"];
    context["truncated"] == false && node["id"] == json!(id) && node["task_state"] == "draft"
}
impl Register {
    pub fn valid(&self) -> bool {
        let generation = self.generation > 0 && !self.ordinary_default;
        let behavior = (1..=4096).contains(&self.permanent_behavior.len())
            && !self.permanent_behavior.trim().is_empty()
            && !self.permanent_behavior.contains('\0');
        !self.operation_id.is_nil()
            && generation
            && self.valid_ids()
            && behavior
            && self.valid_repositories()
    }

    fn valid_ids(&self) -> bool {
        let ids = [
            self.project_id,
            self.epic_id,
            self.adopt.approval,
            self.adopt.cleanup,
        ];
        ids.iter()
            .enumerate()
            .all(|(i, id)| !id.is_nil() && !ids[..i].contains(id))
    }
    fn valid_repositories(&self) -> bool {
        (1..=32).contains(&self.repositories.len())
            && self
                .repositories
                .iter()
                .all(|(repo, f)| repository(repo) && flag(f))
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

    fn draft(&self, at: Scope<'_>, context: &Value, id: Uuid) -> Option<AdoptedDraft> {
        let node = &context["node"];
        let metadata = node["metadata"].as_object()?;
        let draft = is_draft(context, id);
        let owner = metadata.get("aq:human_actor_id") == Some(&json!(at.actor))
            && metadata.get("aq:project_id") == Some(&json!(self.project_id));
        let original = node["verification"] == "unverified" && node["created_by"] == at.service;
        let adoptable = draft && original && owner && !metadata.contains_key(META);
        if !adoptable {
            return None;
        }
        Some(AdoptedDraft {
            version: node["version"].as_u64()?,
            metadata: metadata.clone(),
        })
    }

    pub fn plan(
        &self,
        at: Scope<'_>,
        epic: &Value,
        [approval, cleanup]: [&Value; 2],
    ) -> Option<Vec<Value>> {
        let open = matches!(epic["task_state"].as_str(), Some("draft" | "active"));
        let project_epic = epic["id"] == json!(self.epic_id)
            && epic["metadata"]["aq:project_id"] == json!(self.project_id);
        if !open || !project_epic {
            return None;
        }
        let has_parent = self
            .adopt
            .cleanup_has_parent(self.epic_id, approval, cleanup)?;
        let approval_version = self.draft(at, approval, self.adopt.approval)?.version;
        let AdoptedDraft {
            version: cleanup_version,
            metadata: mut cleanup_meta,
        } = self.draft(at, cleanup, self.adopt.cleanup)?;
        let actor = at.actor;
        let registration = self.id(at.workspace);
        let closure = Uuid::new_v5(&registration, b"closure-gate");
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
