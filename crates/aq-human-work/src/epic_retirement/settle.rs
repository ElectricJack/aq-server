//! Owner recovery and settlement of an AQ-owned registration (spec 0020).
//!
//! Each action plans one Graph batch from fresh reads. The registration
//! record is the action subject and its version the CAS token. Every task and
//! record an action touches is derived from that registration, never taken
//! from the request, and each must carry the registered owner, project and
//! the configured Graph service's own verification.
use super::META;
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
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}
fn digest(value: &Value) -> String {
    let mut text = String::new();
    canonical(value, &mut text);
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

impl Registration {
    /// Only a registration made by `epic.retirement.register` for this
    /// owner: the Graph-direct workflow's registrations name a Graph actor.
    pub fn load(
        node: &Value,
        workspace: Uuid,
        actor: Uuid,
        service: &str,
        project: Uuid,
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
            spec["adopt"] == true
                && closure == Uuid::new_v5(&id, b"closure-gate")
                && state.get("setup") == Some(&json!(id))
                && state
                    .get("repositories")
                    .and_then(Value::as_object)
                    .is_some_and(|r| {
                        (1..=32).contains(&r.len()) && r.values().all(Value::is_string)
                    })
                && ["merges", "ordinary"]
                    .iter()
                    .all(|key| state.get(*key).is_some_and(Value::is_object)),
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
    fn create(
        &self,
        id: Uuid,
        title: &str,
        content_type: &str,
        event: &Value,
        evidence: Vec<Value>,
    ) -> [Value; 2] {
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
    fn gates<'a>(
        &self,
        approval: &'a Value,
        cleanup: &'a Value,
        closure: &'a Value,
    ) -> Result<[&'a Value; 3], Refusal> {
        static NONE: Value = Value::Null;
        let mut nodes = [&NONE; 3];
        let mut links = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
        for (i, (context, id)) in [
            (approval, self.approval),
            (cleanup, self.cleanup),
            (closure, self.closure),
        ]
        .into_iter()
        .enumerate()
        {
            let node = &context["node"];
            require(
                context["truncated"] == false
                    && self.owned(node, id, "task")
                    && node["metadata"]["aq:human_actor_id"] == json!(self.actor),
                "a retirement task is missing or belongs to another owner or project",
            )?;
            for edge in context["edges"].as_array().ok_or("no dependency context")? {
                let kind = edge["edge_type"].as_str().unwrap_or_default();
                let ends = (id_of(&edge["source"]), id_of(&edge["target"]));
                // Cleanup work may have subtasks; only its own prerequisites
                // and parent are registered.
                if edge["deleted"] != true
                    && matches!(kind, "blocks" | "parent-child")
                    && (ends.1 == Some(id) || (i != 1 && ends.0 == Some(id)))
                {
                    links[i].insert((kind, ends.0, ends.1));
                }
            }
            nodes[i] = node;
        }
        let blocker = ("blocks", Some(self.approval), Some(self.cleanup));
        let child = |id| ("parent-child", Some(self.epic), Some(id));
        require(
            links[0] == BTreeSet::from([blocker])
                && links[1] == BTreeSet::from([blocker, child(self.cleanup)])
                && links[2] == BTreeSet::from([child(self.closure)]),
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

impl Restore {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id]) && self.revision >= 1
    }
    /// Undelete the registration's own closure gate at its guarded revision
    /// through Graph restoration, then verify it again: restoration never
    /// carries a verification badge forward.
    pub fn plan(
        &self,
        reg: &Registration,
        epic: &Value,
        tombstone: &Value,
        snapshot: &Value,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "the registration is closed",
        )?;
        require(
            reg.open_epic(epic),
            "restore the gate of this project's open epic",
        )?;
        let guarded = json!({"aq:project_id":reg.project,"aq:human_actor_id":reg.actor,
            (META):{"registration":reg.id,"role":"closure_gate"}});
        require(
            tombstone["id"] == json!(reg.closure)
                && tombstone["kind"] == "task"
                && tombstone["deleted"] == true
                && tombstone["created_by"] == reg.service.as_str()
                && tombstone["metadata"]["aq:project_id"] == json!(reg.project)
                && reg.registered(tombstone, "closure_gate"),
            "only the registration's deleted closure gate can be restored",
        )?;
        let node = &snapshot["node"];
        require(
            snapshot["relationships_available"] == true
                && node["id"] == json!(reg.closure)
                && node["content_revision"].as_i64() == Some(self.revision)
                && node["kind"] == "task"
                && node["title"] == GATE_TITLE
                && node["task_state"] == "draft"
                && node["metadata"] == guarded
                && trusted(node, &reg.service)
                && node["provenance"]
                    .as_array()
                    .is_some_and(|p| p.contains(&task_evidence(reg.epic, "epic"))),
            "the revision is not the registration's verified closure gate",
        )?;
        let edge = Uuid::new_v5(
            &reg.id,
            format!("parent-child/{}/{}", reg.epic, reg.closure).as_bytes(),
        );
        let links: Vec<_> = snapshot["links"]
            .as_array()
            .ok_or("the revision has no relationship snapshot")?
            .iter()
            .filter(|l| l["deleted"] != true)
            .collect();
        require(
            links.len() == 1
                && links[0]["id"] == json!(edge)
                && links[0]["edge_type"] == "parent-child"
                && links[0]["source"] == json!(reg.epic)
                && links[0]["target"] == json!(reg.closure),
            "the revision lacks exactly its epic relationship",
        )?;
        let deleted = version(tombstone)?;
        Ok(vec![
            guard(&reg.node),
            guard(epic),
            json!({"op":"restore_node","id":reg.closure,"expected_version":deleted,"revision":self.revision,"include_lifecycle":true}),
            verify(
                reg.closure,
                deleted + 1,
                vec![task_evidence(reg.epic, "epic")],
            ),
        ])
    }
}

impl Decide {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id])
            && self.revision >= 1
            && (1..=32).contains(&self.vetted.len())
            && commits(&self.vetted)
            && self.evidence.valid()
    }
    /// Record the owner's decision. Approval completes the approval gate and
    /// releases the existing cleanup task once, in the same batch.
    pub fn plan(
        &self,
        reg: &Registration,
        approval: &Value,
        cleanup: &Value,
        closure: &Value,
    ) -> Planned {
        require(reg.id == self.registration_id, "wrong registration")?;
        require(
            reg.same_repositories(&self.vetted),
            "vet exactly the registered repositories",
        )?;
        require(
            reg.get("approved").is_null() && !reg.closed(),
            "this generation has already released its cleanup",
        )?;
        require(
            u64::try_from(self.revision).ok() == Some(reg.decision_revision() + 1),
            "the decision revision must follow the last decision",
        )?;
        let [approval, cleanup, _] = reg.gates(approval, cleanup, closure)?;
        require(
            approval["task_state"] == "draft"
                && cleanup["task_state"] == "draft"
                && trusted(cleanup, &reg.service),
            "approval and the protected cleanup must both still be draft",
        )?;
        let anchor = reg.anchor(&format!("decision/{}", self.revision));
        let event = json!({"registration":reg.id,"kind":"decision","revision":self.revision,
            "choice":self.choice,"vetted":self.vetted,"evidence":self.evidence.json()});
        let mut evidence = git_evidence(&self.vetted);
        evidence.push(self.evidence.proof("integrated-vetting"));
        let mut operations = reg
            .create(
                anchor,
                "Record owner retirement decision",
                "decision",
                &event,
                evidence,
            )
            .to_vec();
        let mut state = reg.state.clone();
        state.insert("decision".into(), json!(anchor));
        state.insert("decision_revision".into(), json!(self.revision));
        if self.choice == Choice::Approve {
            operations.extend([
                patch(approval, json!({"task_state":"completed",
                    "metadata":marked(approval, json!({"registration":reg.id,"role":"approval","decision":anchor}))?})),
                verify(reg.approval, version(approval)? + 1, vec![self.evidence.proof("integrated-vetting")]),
                patch(cleanup, json!({"task_state":"active",
                    "metadata":marked(cleanup, json!({"registration":reg.id,"role":"cleanup","approval":anchor}))?})),
            ]);
            state.insert("approved".into(), json!(anchor));
        }
        operations.extend(reg.save(state)?);
        Ok(operations)
    }
}

fn https(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        !authority.is_empty()
            && !authority.contains('@')
            && url.bytes().all(|b| b.is_ascii_graphic())
    })
}

impl Evidence {
    pub fn valid(&self) -> bool {
        let repository = self
            .repository
            .as_ref()
            .is_some_and(|r| (1..=96).contains(&r.len()));
        let sha = self.sha.as_deref().is_some_and(commit);
        let fields = match self.kind {
            EvidenceKind::Merge => {
                repository
                    && sha
                    && self
                        .pull_request
                        .as_deref()
                        .is_some_and(|u| u.len() <= 2048 && https(u))
                    && self.supersedes_merge.is_none_or(|id| !id.is_nil())
                    && self.passed.is_none()
                    && self.flag_absent.is_none()
                    && self.revisions.is_none()
                    && self.builds.is_none()
            }
            EvidenceKind::Ordinary => {
                repository
                    && sha
                    && self.passed.is_some()
                    && self.flag_absent.is_some()
                    && self.pull_request.is_none()
                    && self.supersedes_merge.is_none()
                    && self.revisions.is_none()
                    && self.builds.is_none()
            }
            EvidenceKind::Integrated => {
                self.revisions
                    .as_ref()
                    .is_some_and(|r| (1..=32).contains(&r.len()) && commits(r))
                    && self.builds.as_ref().is_some_and(|b| {
                        (1..=32).contains(&b.len()) && b.values().all(Artifact::valid)
                    })
                    && self.passed.is_some()
                    && self.repository.is_none()
                    && self.sha.is_none()
                    && self.pull_request.is_none()
                    && self.supersedes_merge.is_none()
                    && self.flag_absent.is_none()
            }
        };
        ids_valid([self.operation_id, self.project_id, self.registration_id])
            && self.approval_revision >= 1
            && self.evidence.valid()
            && fields
    }
    fn event(&self, reg: &Registration) -> Value {
        let mut event = json!({"registration":reg.id,"kind":self.kind,
            "approval_revision":self.approval_revision,"evidence":self.evidence.json()});
        let map = event.as_object_mut().expect("object");
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                map.insert(key.into(), value);
            }
        };
        put("repository", self.repository.as_ref().map(|v| json!(v)));
        put("pull_request", self.pull_request.as_ref().map(|v| json!(v)));
        put("sha", self.sha.as_ref().map(|v| json!(v)));
        put("supersedes_merge", self.supersedes_merge.map(|v| json!(v)));
        put("flag_absent", self.flag_absent.map(|v| json!(v)));
        put("revisions", self.revisions.as_ref().map(|v| json!(v)));
        put(
            "builds",
            self.builds
                .as_ref()
                .map(|b| b.iter().map(|(r, a)| (r.clone(), a.json())).collect()),
        );
        put("passed", self.passed.map(|v| json!(v)));
        event
    }
    /// Record one owner-attested cleanup receipt against the approved
    /// decision: a merge, the ordinary build at that merge, or the integrated
    /// check over exactly the passing flag-free ordinary builds.
    pub fn plan(
        &self,
        reg: &Registration,
        gates: [&Value; 3],
        records: &BTreeMap<Uuid, Value>,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "this epic retirement is closed",
        )?;
        let (_, decision) = reg.approved(records)?;
        require(
            decision["revision"].as_i64() == Some(i64::from(self.approval_revision)),
            "evidence must name the approved decision revision",
        )?;
        let [approval, _, _] = reg.gates(gates[0], gates[1], gates[2])?;
        require(
            approval["task_state"] == "completed",
            "the approval gate has not released cleanup",
        )?;
        let current = reg.evidence(records)?;
        let event = self.event(reg);
        let anchor = reg.anchor(&format!("evidence/{}", digest(&event)));
        let mut state = reg.state.clone();
        let commits: BTreeMap<String, String> = match self.kind {
            EvidenceKind::Merge | EvidenceKind::Ordinary => {
                let repo = self.repository.clone().unwrap_or_default();
                let sha = self.sha.clone().unwrap_or_default();
                require(
                    reg.repositories().contains_key(repo.as_str()),
                    "the repository is not registered",
                )?;
                if self.kind == EvidenceKind::Merge {
                    require(
                        reg.get("merges")[&repo]
                            == self.supersedes_merge.map_or(Value::Null, |id| json!(id)),
                        "a merge must supersede the repository's current receipt",
                    )?;
                    require(
                        current
                            .merges
                            .get(&repo)
                            .is_none_or(|m| m["sha"] != sha.as_str()),
                        "a repair merge must have a different SHA",
                    )?;
                    let mut merges = reg.get("merges").as_object().cloned().unwrap_or_default();
                    merges.insert(repo.clone(), json!(anchor));
                    let mut ordinary = reg.get("ordinary").as_object().cloned().unwrap_or_default();
                    ordinary.remove(&repo);
                    state.insert("merges".into(), Value::Object(merges));
                    state.insert("ordinary".into(), Value::Object(ordinary));
                    state.insert("integrated".into(), Value::Null);
                } else {
                    require(
                        current
                            .merges
                            .get(&repo)
                            .is_some_and(|m| m["sha"] == sha.as_str()),
                        "the ordinary build must match the recorded merge SHA",
                    )?;
                    let mut ordinary = reg.get("ordinary").as_object().cloned().unwrap_or_default();
                    ordinary.insert(repo.clone(), json!(anchor));
                    state.insert("ordinary".into(), Value::Object(ordinary));
                }
                BTreeMap::from([(repo, sha)])
            }
            EvidenceKind::Integrated => {
                let merged = current.merge_shas();
                require(
                    json!(self.revisions) == json!(merged)
                        && merged
                            .keys()
                            .map(String::as_str)
                            .eq(reg.repositories().into_keys()),
                    "the integrated check must name every registered repository's merge SHA",
                )?;
                let builds: BTreeMap<_, _> = current
                    .ordinary
                    .iter()
                    .filter(|(_, b)| b["passed"] == true && b["flag_absent"] == true)
                    .map(|(repo, b)| (repo.clone(), b["evidence"].clone()))
                    .collect();
                require(
                    event["builds"] == json!(builds) && builds.keys().eq(merged.keys()),
                    "the integrated check must name the passing flag-free ordinary builds",
                )?;
                state.insert("integrated".into(), json!(anchor));
                self.revisions.clone().unwrap_or_default()
            }
        };
        let mut evidence = git_evidence(&commits);
        evidence.push(self.evidence.proof(match self.kind {
            EvidenceKind::Merge => "merge",
            EvidenceKind::Ordinary => "ordinary",
            EvidenceKind::Integrated => "integrated",
        }));
        let mut operations = reg
            .create(
                anchor,
                "Record epic retirement evidence",
                "reference",
                &event,
                evidence,
            )
            .to_vec();
        operations.extend(reg.save(state)?);
        Ok(operations)
    }
}

/// What still keeps the epic open, first item only.
fn outstanding(
    reg: &Registration,
    recorded: &Recorded<'_>,
    cleanup: &Value,
    tree: &Value,
) -> Result<(), Refusal> {
    for repo in reg.repositories().into_keys() {
        let merge = recorded
            .merges
            .get(repo)
            .ok_or("a cleanup merge is pending")?;
        let build = recorded
            .ordinary
            .get(repo)
            .filter(|b| b["sha"] == merge["sha"])
            .ok_or("an ordinary build is pending")?;
        require(
            build["passed"] == true && build["flag_absent"] == true,
            "an ordinary build or flag removal check failed",
        )?;
    }
    let integrated = recorded
        .integrated
        .as_ref()
        .ok_or("the integrated acceptance check is pending")?;
    require(
        integrated["passed"] == true,
        "the integrated acceptance check failed",
    )?;
    let builds: Map<String, Value> = recorded
        .ordinary
        .iter()
        .map(|(repo, b)| (repo.clone(), b["evidence"].clone()))
        .collect();
    require(
        integrated["revisions"] == json!(recorded.merge_shas())
            && integrated["builds"] == Value::Object(builds),
        "the integrated check no longer matches the ordinary builds",
    )?;
    require(
        cleanup["task_state"] == "completed",
        "the cleanup task has not completed through normal delivery",
    )?;
    require(
        tree["truncated"] == false && tree["node"]["id"] == json!(reg.epic),
        "the epic task list is unavailable",
    )?;
    for child in tree["children"]
        .as_array()
        .ok_or("the epic task list is unavailable")?
    {
        let task = &child["node"];
        let own = [json!(reg.cleanup), json!(reg.closure)].contains(&task["id"]);
        require(
            own || matches!(task["task_state"].as_str(), Some("completed" | "canceled")),
            "another epic task is still open",
        )?;
    }
    Ok(())
}

impl Close {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id])
    }
    /// Complete the closure gate and the epic atomically with a verified
    /// closure receipt, only after the approved decision, every registered
    /// repository's merge and passing flag-free ordinary build, the passing
    /// integrated check over exactly those builds, and the cleanup task's
    /// ordinary completion.
    pub fn plan(
        &self,
        reg: &Registration,
        epic: &Value,
        gates: [&Value; 3],
        records: &BTreeMap<Uuid, Value>,
        tree: &Value,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "this epic retirement is closed",
        )?;
        let [approval, cleanup, closure] = reg.gates(gates[0], gates[1], gates[2])?;
        let (decision, approved) = reg.approved(records)?;
        require(
            approval["task_state"] == "completed" && closure["task_state"] == "draft",
            "the retirement gates are in the wrong state for closure",
        )?;
        let recorded = reg.evidence(records)?;
        outstanding(reg, &recorded, cleanup, tree)?;
        require(
            reg.open_epic(epic),
            "the epic was closed outside the workflow",
        )?;
        let anchor = reg.anchor("close");
        let event = json!({"registration":reg.id,"kind":"closure","approval_revision":approved["revision"],
            "decision":decision["id"],"cleanup":reg.cleanup,"merges":reg.get("merges"),
            "ordinary":reg.get("ordinary"),"integrated":reg.get("integrated")});
        let integrated = recorded
            .integrated
            .as_ref()
            .ok_or("the integrated acceptance check is pending")?;
        let proof = json!({"kind":"artifact","source_id":"integrated-acceptance",
            "artifact_id":integrated["evidence"]["artifact_id"],"sha256":integrated["evidence"]["sha256"]});
        let mut operations: Vec<Value> = [approval, cleanup, decision]
            .into_iter()
            .chain(recorded.nodes.iter().copied())
            .map(guard)
            .collect();
        operations.extend(reg.create(
            anchor,
            "Close epic feature retirement",
            "decision",
            &event,
            vec![proof.clone(), task_evidence(reg.cleanup, "cleanup")],
        ));
        operations.extend([
            patch(closure, json!({"task_state":"completed",
                "metadata":marked(closure, json!({"registration":reg.id,"role":"closure_gate","receipt":anchor}))?})),
            verify(reg.closure, version(closure)? + 1, vec![proof]),
            patch(epic, json!({"task_state":"completed"})),
        ]);
        let mut state = reg.state.clone();
        state.insert("closed".into(), json!(anchor));
        operations.extend(reg.save(state)?);
        Ok(operations)
    }
}

#[cfg(test)]
mod tests;
