//! Authorized API01 startup projection. Raw daemon graph metadata/provenance is
//! retained centrally, never forwarded wholesale into the model's context.
use crate::agent_access::scheduling::TaskContext;
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::{GraphAccessScope, GraphEdgeType};
use aq_session_api::Permission;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid or incomplete required task context")]
    Invalid,
    #[error("task context observation is not fresh")]
    Expired,
    #[error("required context exceeds the owned typed read scope")]
    Permission,
}
pub struct Projection {
    pub context: Value,
    pub bytes: Vec<u8>,
    pub artifact: ArtifactRef,
    pub scope: GraphAccessScope,
    pub source_response_digest: String,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
fn positive(value: &Value) -> Result<String, Error> {
    value
        .as_u64()
        .filter(|n| *n > 0 && *n <= i64::MAX as u64)
        .map(|n| n.to_string())
        .ok_or(Error::Invalid)
}
fn id(value: &Value) -> Result<Uuid, Error> {
    value
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|u| !u.is_nil())
        .ok_or(Error::Invalid)
}
fn node(
    raw: &Value,
    workspace: Uuid,
    project: Uuid,
    permissions: &[Permission],
) -> Result<Value, Error> {
    if id(&raw["workspace_id"])? != workspace {
        return Err(Error::Invalid);
    }
    let operation = match raw["kind"].as_str() {
        Some("task") => "task.read",
        Some("knowledge") => "knowledge.read",
        _ => return Err(Error::Invalid),
    };
    if !permissions
        .iter()
        .any(|p| p.operation == operation && p.scope_reference == format!("project:{project}"))
    {
        return Err(Error::Permission);
    }
    let mut value = serde_json::Map::new();
    for field in [
        "id",
        "kind",
        "title",
        "body",
        "acceptance_criteria",
        "labels",
        "task_state",
        "verification",
        "content_type",
        "retired",
        "deleted",
    ] {
        value.insert(field.into(), raw.get(field).ok_or(Error::Invalid)?.clone());
    }
    for field in ["version", "content_revision"] {
        value.insert(field.into(), json!(positive(&raw[field])?));
    }
    let value = Value::Object(value);
    if !aq_agent_api::valid("ScopedNode", &value) {
        return Err(Error::Invalid);
    }
    Ok(value)
}
/// The supplied project/repository/workspace/permissions are daemon-owned
/// binding facts, not caller authority. Final admission must fence them again.
pub fn project(
    observation: &TaskContext,
    project_id: Uuid,
    repository_id: Uuid,
    workspace_id: Uuid,
    permissions: &[Permission],
    now_ms: u64,
) -> Result<Projection, Error> {
    if [project_id, repository_id, workspace_id]
        .iter()
        .any(Uuid::is_nil)
        || observation.revision.workspace_id != workspace_id
        || observation.revision.validate().is_err()
        || observation.truncated
        || observation.context["truncated"] != false
        || observation.raw_json.len() > 1_048_576
        || permissions.len() > 128
    {
        return Err(Error::Invalid);
    }
    let stamp = &observation.stamp;
    if stamp.requested_at_ms == 0
        || stamp.requested_at_ms > stamp.received_at_ms
        || stamp.received_at_ms > now_ms
        || stamp.expires_at_ms <= stamp.requested_at_ms
        || stamp.expires_at_ms - stamp.requested_at_ms > 30000
        || now_ms >= stamp.expires_at_ms
    {
        return Err(Error::Expired);
    }
    if Digest::of(observation.raw_json.as_bytes()).as_str() != observation.digest
        || stamp.response_digest != observation.digest
        || serde_json::from_str::<Value>(&observation.raw_json).map_err(|_| Error::Invalid)?
            != observation.context
    {
        return Err(Error::Invalid);
    }
    let raw = &observation.context;
    if id(&raw["node"]["id"])? != observation.revision.task_id
        || raw["node"]["version"] != observation.revision.version
        || raw["node"]["content_revision"] != observation.revision.content_revision
        || raw["node"]["kind"] != "task"
        || raw["node"]["task_state"] != "active"
        || raw["node"]["retired"] != false
        || raw["node"]["deleted"] != false
    {
        return Err(Error::Invalid);
    }
    let extra = raw["nodes"].as_array().ok_or(Error::Invalid)?;
    let pins = raw["pinned_revisions"].as_array().ok_or(Error::Invalid)?;
    let edges = raw["edges"].as_array().ok_or(Error::Invalid)?;
    if extra.len() >= 64 || pins.len() > 64 || edges.len() > 256 {
        return Err(Error::Invalid);
    }
    // An agent is told about its own task (spec 0014 §6.3). The tasks this
    // task blocks are other agents' work: they are left out of the startup
    // text and of the read scope, with every edge and pin that names them.
    // What the task depends on stays.
    let root = observation.revision.task_id;
    let mut dependents = BTreeSet::new();
    for edge in edges {
        if edge["edge_type"] == "blocks" && id(&edge["source"])? == root {
            dependents.insert(id(&edge["target"])?);
        }
    }
    dependents.remove(&root);
    let names_dependent =
        |edge: &Value| -> Result<bool, Error> {
            Ok(dependents.contains(&id(&edge["source"])?)
                || dependents.contains(&id(&edge["target"])?))
        };
    let mut nodes = vec![node(&raw["node"], workspace_id, project_id, permissions)?];
    let mut current = BTreeSet::from([observation.revision.task_id]);
    for record in extra {
        if dependents.contains(&id(&record["id"])?) {
            continue;
        }
        if !current.insert(id(&record["id"])?) {
            return Err(Error::Invalid);
        }
        nodes.push(node(record, workspace_id, project_id, permissions)?);
    }
    let mut pinned = Vec::new();
    let mut pin_ids = BTreeSet::new();
    let mut record_ids = current.clone();
    for record in pins {
        if dependents.contains(&id(&record["id"])?) {
            continue;
        }
        let key = (id(&record["id"])?, positive(&record["content_revision"])?);
        if !current.contains(&key.0) || !pin_ids.insert(key.clone()) {
            return Err(Error::Invalid);
        }
        record_ids.insert(key.0);
        pinned.push(node(record, workspace_id, project_id, permissions)?);
    }
    let mut projected_edges = Vec::new();
    let mut edge_ids = BTreeSet::new();
    let mut edge_types = BTreeSet::new();
    let mut expected_pins = BTreeSet::new();
    for edge in edges {
        if names_dependent(edge)? {
            continue;
        }
        if id(&edge["workspace_id"])? != workspace_id
            || edge["deleted"] != false
            || !edge_ids.insert(id(&edge["id"])?)
        {
            return Err(Error::Invalid);
        }
        let mut value = serde_json::Map::new();
        for field in ["id", "source", "target", "edge_type", "description"] {
            value.insert(field.into(), edge.get(field).ok_or(Error::Invalid)?.clone());
        }
        value.insert("version".into(), json!(positive(&edge["version"])?));
        for (endpoint, revision) in [("source", "source_revision"), ("target", "target_revision")] {
            let endpoint_id = id(&edge[endpoint])?;
            if !current.contains(&endpoint_id) {
                return Err(Error::Invalid);
            }
            let raw_revision = edge.get(revision).ok_or(Error::Invalid)?;
            let revision_value = if raw_revision.is_null() {
                Value::Null
            } else {
                let v = positive(raw_revision)?;
                expected_pins.insert((endpoint_id, v.clone()));
                json!(v)
            };
            value.insert(revision.into(), revision_value);
        }
        let value = Value::Object(value);
        if !aq_agent_api::valid("ScopedEdge", &value) {
            return Err(Error::Invalid);
        }
        edge_types.insert(serde_json::to_string(&value["edge_type"]).map_err(|_| Error::Invalid)?);
        projected_edges.push(value);
    }
    if pin_ids != expected_pins {
        return Err(Error::Invalid);
    }
    let sort_nodes = |records: &mut Vec<Value>| {
        records.sort_by(|a, b| {
            a["id"].as_str().cmp(&b["id"].as_str()).then_with(|| {
                a["content_revision"]
                    .as_str()
                    .cmp(&b["content_revision"].as_str())
            })
        })
    };
    sort_nodes(&mut nodes);
    sort_nodes(&mut pinned);
    projected_edges.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let observed = chrono::DateTime::from_timestamp_millis(
        i64::try_from(stamp.requested_at_ms).map_err(|_| Error::Invalid)?,
    )
    .ok_or(Error::Invalid)?
    .to_rfc3339();
    let mut context = json!({"observed_at":observed,"projection":"authorized_scope","root_ids":[observation.revision.task_id],"nodes":nodes,"edges":projected_edges,"pinned_revisions":pinned,"coverage":{"status":"complete","complete_for":"authorized_projection","reasons":[]}});
    if !aq_agent_api::valid("ContextSnapshot", &context) {
        return Err(Error::Invalid);
    }
    context.sort_all_objects();
    let bytes = serde_json::to_vec(&context).map_err(|_| Error::Invalid)?;
    if bytes.len() > 1_048_576 {
        return Err(Error::Invalid);
    }
    let digest = Digest::of(&bytes);
    let scope = GraphAccessScope {
        repository_id,
        workspace_id,
        task_id: observation.revision.task_id,
        record_ids: record_ids.into_iter().collect(),
        edge_types: edge_types
            .into_iter()
            .map(|s| serde_json::from_str::<GraphEdgeType>(&s).map_err(|_| Error::Invalid))
            .collect::<Result<_, _>>()?,
    };
    Ok(Projection {
        artifact: ArtifactRef {
            id: Text::new(format!("aq.authorized-task-context/{}", digest.as_str()))
                .map_err(|_| Error::Invalid)?,
            version: Text::new("1").map_err(|_| Error::Invalid)?,
            digest,
        },
        context,
        bytes,
        scope,
        source_response_digest: observation.digest.clone(),
        observed_at_ms: stamp.requested_at_ms,
        expires_at_ms: stamp.expires_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_access::scheduling::GraphReadStamp;
    use aq_admission::TaskRevision;
    fn uid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }
    fn raw_node(n: u128, kind: &str) -> Value {
        json!({"id":uid(n),"workspace_id":uid(2),"kind":kind,"version":1,"content_revision":1,"title":"Scoped record","body":"Visible work","acceptance_criteria":"Verify","labels":[],"task_state":if kind=="task"{Some("active")}else{None},"verification":null,"content_type":null,"retired":false,"deleted":false,"metadata":{"fraction":1.5,"private_extra":"not-for-model"},"created_by":"not-for-model","provenance":["not-for-model"]})
    }
    fn observation(mixed: bool) -> TaskContext {
        let context = json!({"node":raw_node(3,"task"),"nodes":if mixed{vec![raw_node(4,"knowledge")]}else{vec![]},"edges":[],"pinned_revisions":[],"truncated":false});
        let raw_json = serde_json::to_string(&context).unwrap();
        let digest = Digest::of(raw_json.as_bytes()).as_str().to_string();
        TaskContext {
            revision: TaskRevision {
                workspace_id: uid(2),
                task_id: uid(3),
                version: 1,
                content_revision: 1,
            },
            context,
            raw_json,
            digest: digest.clone(),
            truncated: false,
            stamp: GraphReadStamp {
                requested_at_ms: 1000,
                received_at_ms: 1100,
                expires_at_ms: 31000,
                response_digest: digest,
            },
        }
    }
    fn permissions() -> Vec<Permission> {
        ["task.read", "knowledge.read"]
            .into_iter()
            .map(|operation| Permission {
                operation: operation.into(),
                scope_reference: format!("project:{}", uid(1)),
            })
            .collect()
    }
    fn refresh(observation: &mut TaskContext) {
        observation.raw_json = serde_json::to_string(&observation.context).unwrap();
        observation.digest = Digest::of(observation.raw_json.as_bytes()).as_str().into();
        observation.stamp.response_digest = observation.digest.clone();
    }
    #[test]
    fn only_authorized_api01_fields_enter_startup_and_original_age_is_preserved() {
        let input = observation(true);
        let projected = project(&input, uid(1), uid(41), uid(2), &permissions(), 2000).unwrap();
        assert!(
            !String::from_utf8(projected.bytes.clone())
                .unwrap()
                .contains("not-for-model")
        );
        assert!(
            projected.context["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|node| node.get("metadata").is_none() && node.get("workspace_id").is_none())
        );
        assert_eq!(projected.context["nodes"][0]["version"], "1");
        assert_eq!(projected.scope.record_ids, vec![uid(3), uid(4)]);
        assert_eq!(projected.observed_at_ms, 1000);
        assert_eq!(projected.expires_at_ms, 31000);
        assert_eq!(projected.source_response_digest, input.digest);
        assert_eq!(projected.artifact.digest, Digest::of(&projected.bytes));
        assert!(aq_agent_api::valid("ContextSnapshot", &projected.context));
    }
    #[test]
    fn mixed_context_never_expands_task_only_or_knowledge_only_permission() {
        for operation in ["task.read", "knowledge.read"] {
            let permission = Permission {
                operation: operation.into(),
                scope_reference: format!("project:{}", uid(1)),
            };
            assert!(matches!(
                project(
                    &observation(true),
                    uid(1),
                    uid(41),
                    uid(2),
                    &[permission],
                    2000
                ),
                Err(Error::Permission)
            ));
        }
        let alias = vec![Permission {
            operation: "task.read".into(),
            scope_reference: "project:example".into(),
        }];
        assert!(matches!(
            project(&observation(false), uid(1), uid(41), uid(2), &alias, 2000),
            Err(Error::Permission)
        ));
    }
    #[test]
    fn expired_partial_forged_and_foreign_observations_refuse() {
        let input = observation(false);
        assert!(matches!(
            project(&input, uid(1), uid(41), uid(2), &permissions(), 31000),
            Err(Error::Expired)
        ));
        let mut partial = observation(false);
        partial.truncated = true;
        assert!(project(&partial, uid(1), uid(41), uid(2), &permissions(), 2000).is_err());
        let mut changed = observation(false);
        changed.context["node"]["body"] = json!("changed after read");
        assert!(project(&changed, uid(1), uid(41), uid(2), &permissions(), 2000).is_err());
        let mut foreign = observation(false);
        foreign.context["node"]["workspace_id"] = json!(uid(99));
        refresh(&mut foreign);
        assert!(project(&foreign, uid(1), uid(41), uid(2), &permissions(), 2000).is_err());
        let mut stale = observation(false);
        stale.context["node"]["version"] = json!(2);
        refresh(&mut stale);
        assert!(project(&stale, uid(1), uid(41), uid(2), &permissions(), 2000).is_err());
    }
    #[test]
    fn a_task_it_blocks_is_left_out_and_what_it_depends_on_stays() {
        let task = |n: u128, body: &str| {
            let mut node = raw_node(n, "task");
            node["body"] = json!(body);
            node
        };
        let blocks = |edge: u128, source: u128, target: u128| json!({"id":uid(edge),"workspace_id":uid(2),"source":uid(source),"target":uid(target),"edge_type":"blocks","version":1,"source_revision":null,"target_revision":null,"description":null,"deleted":false});
        let mut input = observation(false);
        // 5 blocks the task (a prerequisite); the task blocks 6 (a dependent).
        input.context["nodes"] = json!([
            task(5, "Prerequisite work, already done"),
            task(6, "Dependent work for another agent")
        ]);
        input.context["edges"] = json!([blocks(8, 5, 3), blocks(9, 3, 6)]);
        refresh(&mut input);
        let projected = project(&input, uid(1), uid(41), uid(2), &permissions(), 2000).unwrap();
        let text = String::from_utf8(projected.bytes.clone()).unwrap();
        assert!(text.contains("Prerequisite work, already done"));
        assert!(!text.contains("Dependent work for another agent"));
        assert!(!text.contains(&uid(6).to_string()));
        assert_eq!(projected.scope.record_ids, vec![uid(3), uid(5)]);
        assert_eq!(projected.context["edges"].as_array().unwrap().len(), 1);
        assert_eq!(projected.context["edges"][0]["id"], json!(uid(8)));
        assert!(aq_agent_api::valid("ContextSnapshot", &projected.context));
    }
    #[test]
    fn scoped_edges_require_exact_authorized_endpoints_and_pinned_revisions() {
        let mut input = observation(true);
        input.context["edges"] = json!([{"id":uid(8),"workspace_id":uid(2),"source":uid(3),"target":uid(4),"edge_type":"references","version":1,"source_revision":null,"target_revision":1,"description":null,"deleted":false,"metadata":{"ignored":"not-for-model"}}]);
        input.context["pinned_revisions"] = json!([raw_node(4, "knowledge")]);
        refresh(&mut input);
        let result = project(&input, uid(1), uid(41), uid(2), &permissions(), 2000).unwrap();
        assert_eq!(result.context["edges"][0]["target_revision"], "1");
        assert!(result.context["edges"][0].get("metadata").is_none());
        input.context["pinned_revisions"] = json!([]);
        refresh(&mut input);
        assert!(project(&input, uid(1), uid(41), uid(2), &permissions(), 2000).is_err());
    }
}
