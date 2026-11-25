//! Owner recovery and settlement of an AQ-owned registration (spec 0020).
//!
//! Each action plans one Graph batch from fresh reads. The registration
//! record is the action subject and its version the CAS token. Every task and
//! record an action touches is derived from that registration, never taken
//! from the request, and each must carry the registered owner, project and
//! the configured Graph service's own verification.
use super::{META, Scope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const RESTORE: &str = "epic.retirement.restore";
pub const DECIDE: &str = "epic.retirement.decide";
pub const EVIDENCE: &str = "epic.retirement.evidence";
pub const CLOSE: &str = "epic.retirement.close";
const GATE_TITLE: &str = "Verify epic retirement evidence";

/// Why a plan was refused; the action reports it as an invalid request.
pub type Refusal = &'static str;
type Planned = Result<Vec<Value>, Refusal>;

fn require(yes: bool, why: Refusal) -> Result<(), Refusal> {
    if yes { Ok(()) } else { Err(why) }
}
fn id_of(value: &Value) -> Option<Uuid> {
    value.as_str()?.parse().ok()
}
fn hex(text: &str, lengths: &[usize]) -> bool {
    lengths.contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// A full, nonzero Git commit identity.
fn commit(text: &str) -> bool {
    hex(text, &[40, 64]) && text.bytes().any(|b| b != b'0')
}
fn commits(value: &BTreeMap<String, String>) -> bool {
    value.values().all(|sha| commit(sha))
}
/// Verified by the configured service, which created it, and still live.
fn trusted(node: &Value, service: &str) -> bool {
    node["created_by"] == service
        && node["verification"] == "verified"
        && node["last_verified_by"] == service
        && node["deleted"] == false
        && node["retired"] == false
}
fn digest(value: &Value) -> String {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    let text = canonical.to_string();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn patch(node: &Value, patch: Value) -> Value {
    json!({"op":"update_node","id":node["id"],"expected_version":node["version"],"patch":patch})
}
fn guard(node: &Value) -> Value {
    patch(node, json!({}))
}
fn verify(id: Uuid, version: u64, evidence: Vec<Value>) -> Value {
    json!({"op":"verify_node","id":id,"expected_version":version,"verification":"verified","evidence":evidence})
}
fn task_evidence(task: Uuid, label: &str) -> Value {
    json!({"kind":"task","source_id":label,"task_id":task})
}
fn git_evidence(revisions: &BTreeMap<String, String>) -> Vec<Value> {
    revisions
        .iter()
        .map(|(repo, sha)| json!({"kind":"git","source_id":format!("revision/{repo}"),"repository":repo,"commit":sha,"path":"."}))
        .collect()
}
fn version(node: &Value) -> Result<u64, Refusal> {
    node["version"]
        .as_u64()
        .ok_or("record version is unavailable")
}
fn metadata(node: &Value) -> Result<&Map<String, Value>, Refusal> {
    node["metadata"]
        .as_object()
        .ok_or("record metadata is unavailable")
}
/// Metadata with the retirement marker replaced; other keys are kept.
fn marked(node: &Value, marker: Value) -> Result<Value, Refusal> {
    let mut metadata = metadata(node)?.clone();
    metadata.insert(META.into(), marker);
    Ok(Value::Object(metadata))
}

/// A retained artifact identity the owner inspected.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub artifact_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
}
impl Artifact {
    fn valid(&self) -> bool {
        !self.artifact_id.is_nil() && hex(&self.sha256, &[64])
    }
    fn json(&self) -> Value {
        json!({"artifact_id":self.artifact_id,"sha256":self.sha256})
    }
    fn proof(&self, label: &str) -> Value {
        json!({"kind":"artifact","source_id":label,"artifact_id":self.artifact_id,"sha256":self.sha256})
    }
}

/// The registration record, checked for this owner, project and service.
pub struct Registration {
    pub id: Uuid,
    pub version: u64,
    pub project: Uuid,
    pub actor: Uuid,
    pub epic: Uuid,
    pub approval: Uuid,
    pub cleanup: Uuid,
    pub closure: Uuid,
    node: Value,
    state: Map<String, Value>,
    service: String,
}

fn owner_setup(state: &Map<String, Value>, spec: &Value, id: Uuid, closure: Uuid) -> bool {
    spec["adopt"] == true
        && closure == Uuid::new_v5(&id, b"closure-gate")
        && state.get("setup") == Some(&json!(id))
        && state
            .get("repositories")
            .and_then(Value::as_object)
            .is_some_and(|r| (1..=32).contains(&r.len()) && r.values().all(Value::is_string))
        && ["merges", "ordinary"]
            .iter()
            .all(|key| state.get(*key).is_some_and(Value::is_object))
}
type GateLink<'a> = (&'a str, Option<Uuid>, Option<Uuid>);
fn gate_link(edge: &Value, id: Uuid, cleanup: bool) -> Option<GateLink<'_>> {
    let kind = edge["edge_type"].as_str().unwrap_or_default();
    let ends = (id_of(&edge["source"]), id_of(&edge["target"]));
    // Cleanup work may have subtasks; only its own prerequisites
    // and parent are registered.
    let structural = edge["deleted"] != true && matches!(kind, "blocks" | "parent-child");
    let relevant = ends.1 == Some(id) || (!cleanup && ends.0 == Some(id));
    (structural && relevant).then_some((kind, ends.0, ends.1))
}
enum Receipt<'a> {
    Decision(&'a Value),
    Evidence(&'a Value),
    Closure(&'a Value),
}

pub struct GateRevision<'a> {
    pub tombstone: &'a Value,
    pub snapshot: &'a Value,
}

pub struct ClosureReads<'a> {
    pub epic: &'a Value,
    pub gates: [&'a Value; 3],
    pub records: &'a BTreeMap<Uuid, Value>,
    pub tree: &'a Value,
}

impl Registration {
    /// Only a registration made by `epic.retirement.register` for this
    /// owner: the Graph-direct workflow's registrations name a Graph actor.
    pub fn load(
        node: &Value,
        Scope {
            workspace,
            actor,
            service,
            project,
        }: Scope<'_>,
    ) -> Result<Self, Refusal> {
        let meta = metadata(node)?;
        let state = meta
            .get(META)
            .and_then(Value::as_object)
            .ok_or("not an epic retirement registration")?;
        let spec = state.get("spec").unwrap_or(&Value::Null);
        let field = |key: &str| id_of(&spec[key]).ok_or("registration names no task");
        let (epic, approval, cleanup, closure) = (
            field("epic_id")?,
            field("approval")?,
            field("cleanup")?,
            field("closure_gate")?,
        );
        let generation = spec["generation"].as_u64().ok_or("no generation")?;
        let id = Uuid::new_v5(
            &workspace,
            format!("aq/epic-retirement/1/{project}/{epic}/{generation}").as_bytes(),
        );
        require(
            node["id"] == json!(id)
                && node["kind"] == "knowledge"
                && node["content_type"] == "policy",
            "the subject is not this project's registration",
        )?;
        require(
            trusted(node, service),
            "registration lacks service verification",
        )?;
        require(
            meta.get("aq:project_id") == Some(&json!(project))
                && spec["project_id"] == json!(project),
            "registration belongs to another project",
        )?;
        require(
            meta.get("aq:human_actor_id") == Some(&json!(actor))
                && spec["owner_authority"] == json!({"kind":"aq_owner","principal_id":actor}),
            "registration belongs to another owner",
        )?;
        require(
            owner_setup(state, spec, id, closure),
            "registration state is not an AQ owner setup",
        )?;
        Ok(Self {
            id,
            version: version(node)?,
            project,
            actor,
            epic,
            approval,
            cleanup,
            closure,
            node: node.clone(),
            state: state.clone(),
            service: service.into(),
        })
    }
    fn get(&self, key: &str) -> &Value {
        self.state.get(key).unwrap_or(&Value::Null)
    }
    /// The AQ owner operation that created the record, as the record names it.
    pub fn operation(&self) -> Option<Uuid> {
        id_of(&self.node["metadata"]["aq:human_operation_id"])
    }
    fn repositories(&self) -> BTreeMap<&str, &Value> {
        self.get("repositories")
            .as_object()
            .map(|r| r.iter().map(|(k, v)| (k.as_str(), v)).collect())
            .unwrap_or_default()
    }
    fn same_repositories<V>(&self, value: &BTreeMap<String, V>) -> bool {
        value
            .keys()
            .map(String::as_str)
            .eq(self.repositories().into_keys())
    }
    fn decision_revision(&self) -> u64 {
        self.get("decision_revision").as_u64().unwrap_or(0)
    }
    fn anchor(&self, key: &str) -> Uuid {
        Uuid::new_v5(&self.id, key.as_bytes())
    }
    fn closed(&self) -> bool {
        !self.get("closed").is_null()
    }
    /// The decision and evidence records the current state names.
    pub fn anchors(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = ["merges", "ordinary"]
            .iter()
            .flat_map(|key| {
                self.get(key)
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.values())
            })
            .chain([self.get("approved"), self.get("integrated")])
            .filter_map(id_of)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
    fn open_epic(&self, epic: &Value) -> bool {
        epic["id"] == json!(self.epic)
            && epic["kind"] == "task"
            && epic["deleted"] == false
            && epic["metadata"]["aq:project_id"] == json!(self.project)
            && matches!(epic["task_state"].as_str(), Some("draft" | "active"))
    }
    fn owned(&self, node: &Value, id: Uuid, kind: &str) -> bool {
        node["id"] == json!(id)
            && node["kind"] == kind
            && node["deleted"] == false
            && node["metadata"]["aq:project_id"] == json!(self.project)
            && node["created_by"] == self.service.as_str()
    }
    /// A verified record of this registration, owner and project.
    fn record(&self, node: &Value, kind: &str) -> Option<Value> {
        let data = &node["metadata"][META];
        (trusted(node, &self.service)
            && node["metadata"]["aq:project_id"] == json!(self.project)
            && node["metadata"]["aq:human_actor_id"] == json!(self.actor)
            && data["registration"] == json!(self.id)
            && data["kind"] == kind)
            .then(|| data.clone())
    }
    fn create(&self, id: Uuid, receipt: Receipt<'_>, evidence: Vec<Value>) -> [Value; 2] {
        let (title, content_type, event) = match receipt {
            Receipt::Decision(event) => ("Record owner retirement decision", "decision", event),
            Receipt::Evidence(event) => ("Record epic retirement evidence", "reference", event),
            Receipt::Closure(event) => ("Close epic feature retirement", "decision", event),
        };
        [
            json!({"op":"create_node","id":id,"kind":"knowledge","content_type":content_type,"title":title,
                "metadata":{"aq:project_id":self.project,"aq:human_actor_id":self.actor,(META):event}}),
            verify(id, 1, evidence),
        ]
    }
    fn save(&self, state: Map<String, Value>) -> Result<[Value; 2], Refusal> {
        Ok([
            patch(
                &self.node,
                json!({"metadata":marked(&self.node, Value::Object(state))?}),
            ),
            verify(
                self.id,
                self.version + 1,
                vec![task_evidence(self.epic, "epic")],
            ),
        ])
    }

    /// The approval, cleanup and closure tasks with exactly their registered
    /// dependencies, from their `/context` reads.
    fn gate<'a>(
        &self,
        context: &'a Value,
        id: Uuid,
        cleanup: bool,
    ) -> Result<(&'a Value, BTreeSet<GateLink<'a>>), Refusal> {
        let node = &context["node"];
        require(
            context["truncated"] == false
                && self.owned(node, id, "task")
                && node["metadata"]["aq:human_actor_id"] == json!(self.actor),
            "a retirement task is missing or belongs to another owner or project",
        )?;
        let edges = context["edges"].as_array().ok_or("no dependency context")?;
        let links = edges
            .iter()
            .filter_map(|edge| gate_link(edge, id, cleanup))
            .collect();
        Ok((node, links))
    }
    fn gates<'a>(
        &self,
        [approval, cleanup, closure]: [&'a Value; 3],
    ) -> Result<[&'a Value; 3], Refusal> {
        let (approval, a) = self.gate(approval, self.approval, false)?;
        let (cleanup, c) = self.gate(cleanup, self.cleanup, true)?;
        let (closure, g) = self.gate(closure, self.closure, false)?;
        let nodes = [approval, cleanup, closure];
        let blocker = ("blocks", Some(self.approval), Some(self.cleanup));
        let child = |id| ("parent-child", Some(self.epic), Some(id));
        require(
            a == BTreeSet::from([blocker])
                && c == BTreeSet::from([blocker, child(self.cleanup)])
                && g == BTreeSet::from([child(self.closure)]),
            "the retirement dependencies no longer match the registration",
        )?;
        require(
            self.registered(nodes[1], "cleanup")
                && self.registered(nodes[2], "closure_gate")
                && trusted(nodes[2], &self.service),
            "the closure gate lacks its registered verification",
        )?;
        Ok(nodes)
    }
    fn registered(&self, node: &Value, role: &str) -> bool {
        let marker = &node["metadata"][META];
        marker["registration"] == json!(self.id) && marker["role"] == role
    }

    fn approved<'a>(
        &self,
        records: &'a BTreeMap<Uuid, Value>,
    ) -> Result<(&'a Value, Value), Refusal> {
        let anchor = id_of(self.get("approved")).ok_or("approval is still pending")?;
        let node = records
            .get(&anchor)
            .ok_or("the approval decision is unavailable")?;
        let decision = self
            .record(node, "decision")
            .filter(|d| {
                node["id"] == json!(anchor)
                    && d["choice"] == "approve"
                    && d["revision"].as_u64() == Some(self.decision_revision())
                    && d["vetted"].as_object().is_some_and(|v| {
                        v.keys()
                            .map(String::as_str)
                            .eq(self.repositories().into_keys())
                    })
            })
            .ok_or("the approved decision no longer matches the registration")?;
        Ok((node, decision))
    }

    fn evidence<'a>(&self, records: &'a BTreeMap<Uuid, Value>) -> Result<Recorded<'a>, Refusal> {
        let mut nodes = Vec::new();
        let mut read = |anchor: &Value, kind: &str| -> Result<Value, Refusal> {
            let id = id_of(anchor).ok_or("an evidence identity is malformed")?;
            let node = records
                .get(&id)
                .ok_or("retirement evidence is unavailable")?;
            let data = self
                .record(node, kind)
                .filter(|d| {
                    node["id"] == json!(id)
                        && d["approval_revision"].as_u64() == Some(self.decision_revision())
                })
                .ok_or("retirement evidence has the wrong kind or approval revision")?;
            nodes.push(node);
            Ok(data)
        };
        let mut map = |key: &str, kind: &str| -> Result<BTreeMap<String, Value>, Refusal> {
            let mut out = BTreeMap::new();
            for (repo, anchor) in self.get(key).as_object().ok_or("no evidence map")? {
                out.insert(repo.clone(), read(anchor, kind)?);
            }
            Ok(out)
        };
        let merges = map("merges", "merge")?;
        let ordinary = map("ordinary", "ordinary")?;
        let integrated = match self.get("integrated") {
            Value::Null => None,
            anchor => Some(read(anchor, "integrated")?),
        };
        Ok(Recorded {
            merges,
            ordinary,
            integrated,
            nodes,
        })
    }
}

struct Recorded<'a> {
    merges: BTreeMap<String, Value>,
    ordinary: BTreeMap<String, Value>,
    integrated: Option<Value>,
    nodes: Vec<&'a Value>,
}
impl Recorded<'_> {
    fn merge_shas(&self) -> BTreeMap<String, Value> {
        self.merges
            .iter()
            .map(|(repo, m)| (repo.clone(), m["sha"].clone()))
            .collect()
    }
}

/// Fields every settlement action carries; `expected_version` is the
/// registration record's current version.
macro_rules! common {
    ($name:ident { $($field:tt)* }) => {
        #[derive(Clone, Deserialize, Serialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub operation_id: Uuid,
            pub project_id: Uuid,
            /// The `epic.retirement.register` registration record.
            pub registration_id: Uuid,
            #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
            pub expected_version: String,
            $($field)*
        }
    };
}
common!(Restore {
    /// The closure gate's guarded revision from its Graph history: the
    /// registration's verified draft with its epic relationship.
    #[schemars(range(min = 1))]
    pub revision: i64,
});
common!(Decide {
    /// One more than the previous decision's revision, starting at 1.
    #[schemars(range(min = 1, max = 2147483647))]
    pub revision: i32,
    pub choice: Choice,
    /// The vetted full commit SHA of each registered repository.
    pub vetted: BTreeMap<String, String>,
    pub evidence: Artifact,
});
common!(Evidence {
    pub kind: EvidenceKind,
    /// The approved decision's revision.
    #[schemars(range(min = 1, max = 2147483647))]
    pub approval_revision: i32,
    pub evidence: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 96))]
    pub repository: Option<String>,
    /// The merged pull request's HTTPS URL (merge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 9, max = 2048))]
    pub pull_request: Option<String>,
    /// The full merge commit SHA (merge, ordinary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The repository's current merge receipt a repair merge replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_merge: Option<Uuid>,
    /// Whether the check passed (ordinary, integrated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    /// Whether the source, manifests and active workflows no longer
    /// reference the flag (ordinary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag_absent: Option<bool>,
    /// Each registered repository's merge SHA (integrated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revisions: Option<BTreeMap<String, String>>,
    /// Each repository's passing ordinary-build artifact (integrated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds: Option<BTreeMap<String, Artifact>>,
});
common!(Close {});

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Approve,
    RequestChanges,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Merge,
    Ordinary,
    Integrated,
}

fn ids_valid(ids: [Uuid; 3]) -> bool {
    ids.iter().all(|id| !id.is_nil())
}

mod restore;

mod decide;

mod evidence;
mod evidence_fields;

mod close;

#[cfg(test)]
mod tests;
