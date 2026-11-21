//! The planned batches applied to a small in-memory model of the Graph
//! mutation rules they depend on: CAS versions, verification resets,
//! structural edges, parent completion, tombstones and exact-revision restore.
//! Real Graph conformance is covered by the disposable-Graph tests.
use super::super::Register;
use super::*;

const SERVICE: &str = "local-admin";
const REPOS: [&str; 5] = [
    "Acme/server",
    "Acme/dashboard",
    "Acme/worker",
    "Acme/playbooks",
    "Acme/git",
];

#[derive(Clone, Default)]
struct Graph {
    nodes: BTreeMap<Uuid, Value>,
    edges: BTreeMap<Uuid, Value>,
    revisions: BTreeMap<(Uuid, i64), Value>,
}

impl Graph {
    /// Live incident edges; every edge here is structural, so these are
    /// also the owned links of a revision snapshot.
    fn live_edges(&self, id: Uuid) -> Vec<Value> {
        let id = json!(id);
        (self.edges.values())
            .filter(|e| e["deleted"] == false && (e["source"] == id || e["target"] == id))
            .cloned()
            .collect()
    }
    fn bump(&mut self, id: Uuid) {
        let node = self.nodes.get_mut(&id).unwrap();
        node["version"] = json!(node["version"].as_u64().unwrap() + 1);
        node["content_revision"] = json!(node["content_revision"].as_i64().unwrap() + 1);
        let snapshot =
            json!({"node":node.clone(),"links":self.live_edges(id),"relationships_available":true});
        let revision = snapshot["node"]["content_revision"].as_i64().unwrap();
        self.revisions.insert((id, revision), snapshot);
    }
    fn get(&self, id: Uuid) -> Value {
        self.nodes[&id].clone()
    }
    fn context(&self, id: Uuid) -> Value {
        json!({"node":self.get(id),"nodes":[],"edges":self.live_edges(id),"pinned_revisions":[],"truncated":false})
    }
    fn tree(&self, id: Uuid) -> Value {
        let children: Vec<_> = (self.edges.values())
            .filter(|e| e["deleted"] == false && e["edge_type"] == "parent-child" && e["source"] == json!(id))
            .map(|e| json!({"node":self.get(id_of(&e["target"]).unwrap()),"children":[],"truncated":false}))
            .collect();
        json!({"node":self.get(id),"children":children,"truncated":false})
    }
    fn revision(&self, id: Uuid, revision: i64) -> Value {
        self.revisions[&(id, revision)].clone()
    }
    fn check(&self, id: Uuid, op: &Value) -> Result<(), String> {
        let node = self.nodes.get(&id).ok_or("missing")?;
        if node["version"] != op["expected_version"] {
            return Err(format!(
                "stale {id}: {} != {}",
                node["version"], op["expected_version"]
            ));
        }
        Ok(())
    }
    fn apply(&mut self, operations: &[Value]) -> Result<(), String> {
        assert!(!operations.is_empty() && operations.len() <= 100);
        let mut next = self.clone();
        for op in operations {
            next.one(op)?;
        }
        *self = next;
        Ok(())
    }
    fn one(&mut self, op: &Value) -> Result<(), String> {
        let id = id_of(&op["id"]).unwrap();
        match op["op"].as_str().unwrap() {
            "create_node" => {
                if self.nodes.contains_key(&id) {
                    return Err("conflict".into());
                }
                let task = op["kind"] == "task";
                self.nodes.insert(id, json!({"id":id,"kind":op["kind"],"title":op["title"],
                    "content_type":op.get("content_type").cloned().unwrap_or(json!("note")),
                    "metadata":op.get("metadata").cloned().unwrap_or(json!({})),
                    "task_state":if task { op.get("task_state").cloned().unwrap_or(json!("draft")) } else { Value::Null },
                    "verification":"unverified","last_verified_by":null,"created_by":SERVICE,
                    "provenance":[],"deleted":false,"retired":false,"version":0,"content_revision":0}));
                self.bump(id);
            }
            "update_node" => {
                self.check(id, op)?;
                let patch = op["patch"].as_object().unwrap();
                if patch.is_empty() {
                    return Ok(());
                }
                if patch.get("task_state") == Some(&json!("completed")) {
                    let open = (self.edges.values()).any(|e| {
                        e["deleted"] == false
                            && e["edge_type"] == "parent-child"
                            && e["source"] == json!(id)
                            && !matches!(
                                self.nodes[&id_of(&e["target"]).unwrap()]["task_state"].as_str(),
                                Some("completed" | "canceled")
                            )
                    });
                    if open {
                        return Err("open child".into());
                    }
                }
                let node = self.nodes.get_mut(&id).unwrap();
                for (key, value) in patch {
                    node[key] = value.clone();
                }
                if patch.contains_key("metadata") {
                    node["verification"] = json!("unverified");
                    node["last_verified_by"] = Value::Null;
                }
                self.bump(id);
            }
            "verify_node" => {
                self.check(id, op)?;
                let node = self.nodes.get_mut(&id).unwrap();
                node["verification"] = op["verification"].clone();
                node["last_verified_by"] = json!(SERVICE);
                let provenance = node["provenance"].as_array_mut().unwrap();
                for item in op["evidence"].as_array().unwrap() {
                    provenance.retain(|p| p["source_id"] != item["source_id"]);
                    provenance.push(item.clone());
                }
                self.bump(id);
            }
            "add_edge" => {
                let (source, target) =
                    (id_of(&op["source"]).unwrap(), id_of(&op["target"]).unwrap());
                if self.nodes[&source]["deleted"] == true || self.nodes[&target]["deleted"] == true
                {
                    return Err("deleted endpoint".into());
                }
                self.edges.insert(id, json!({"id":id,"edge_type":op["edge_type"],"source":source,"target":target,"deleted":false}));
                self.bump(source);
                self.bump(target);
            }
            "delete_node" => {
                self.check(id, op)?;
                let incident: Vec<_> = self.live_edges(id);
                for edge in &incident {
                    self.edges.get_mut(&id_of(&edge["id"]).unwrap()).unwrap()["deleted"] =
                        json!(true);
                }
                self.nodes.get_mut(&id).unwrap()["deleted"] = json!(true);
                self.bump(id);
                for edge in incident {
                    for end in ["source", "target"] {
                        let other = id_of(&edge[end]).unwrap();
                        if other != id {
                            self.bump(other);
                        }
                    }
                }
            }
            "restore_node" => {
                self.check(id, op)?;
                let snapshot = self.revision(id, op["revision"].as_i64().unwrap());
                let lifecycle = op["include_lifecycle"] == true;
                if self.nodes[&id]["deleted"] == true && !lifecycle {
                    return Err("tombstone".into());
                }
                let mut others = vec![];
                for link in snapshot["links"].as_array().unwrap() {
                    let other = [
                        id_of(&link["source"]).unwrap(),
                        id_of(&link["target"]).unwrap(),
                    ]
                    .into_iter()
                    .find(|end| *end != id)
                    .unwrap();
                    if self.nodes[&other]["deleted"] == true {
                        return Err("deleted endpoint".into());
                    }
                    self.edges.insert(id_of(&link["id"]).unwrap(), link.clone());
                    others.push(other);
                }
                let node = self.nodes.get_mut(&id).unwrap();
                for key in ["title", "metadata"] {
                    node[key] = snapshot["node"][key].clone();
                }
                if lifecycle {
                    for key in ["task_state", "deleted", "retired"] {
                        node[key] = snapshot["node"][key].clone();
                    }
                }
                node["verification"] = json!("unverified");
                node["last_verified_by"] = Value::Null;
                self.bump(id);
                for other in others {
                    self.bump(other);
                }
            }
            other => panic!("unexpected {other}"),
        }
        Ok(())
    }
}

struct Fixture {
    graph: Graph,
    workspace: Uuid,
    project: Uuid,
    actor: Uuid,
    epic: Uuid,
    registration: Uuid,
    closure: Uuid,
    guarded: i64,
}

fn task(graph: &mut Graph, project: Uuid, actor: Uuid, state: &str) -> Uuid {
    let id = Uuid::new_v4();
    graph
        .apply(&[
            json!({"op":"create_node","id":id,"kind":"task","title":"Owner task","task_state":state,
            "metadata":{"aq:project_id":project,"aq:human_actor_id":actor}}),
        ])
        .unwrap();
    id
}

fn repositories() -> BTreeMap<String, String> {
    REPOS
        .iter()
        .map(|r| (r.to_string(), "delivery_playbook".into()))
        .collect()
}

/// A registration through the existing `epic.retirement.register` plan,
/// whose closure gate a later `task.delete` removed.
fn fixture() -> Fixture {
    let mut graph = Graph::default();
    let (workspace, project, actor) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let epic = task(&mut graph, project, actor, "active");
    let approval = task(&mut graph, project, actor, "draft");
    let cleanup = task(&mut graph, project, actor, "draft");
    graph
        .apply(&[json!({"op":"add_edge","id":Uuid::new_v4(),"edge_type":"blocks","source":approval,"target":cleanup})])
        .unwrap();
    let register = Register {
        operation_id: Uuid::new_v4(),
        project_id: project,
        epic_id: epic,
        expected_version: graph.get(epic)["version"].to_string(),
        generation: 1,
        repositories: repositories(),
        ordinary_default: false,
        permanent_behavior: "Deliver through the playbook in ordinary builds".into(),
        adopt: super::super::Adopt { approval, cleanup },
    };
    assert!(register.valid());
    let operations = register
        .plan(
            workspace,
            actor,
            SERVICE,
            &graph.get(epic),
            &graph.context(approval),
            &graph.context(cleanup),
        )
        .unwrap();
    graph.apply(&operations).unwrap();
    let registration = register.id(workspace);
    let closure = Uuid::new_v5(&registration, b"closure-gate");
    let gate = graph.get(closure);
    assert_eq!(gate["verification"], "verified");
    let guarded = gate["content_revision"].as_i64().unwrap();
    // The dashboard's task.delete.
    graph
        .apply(&[json!({"op":"delete_node","id":closure,"expected_version":gate["version"]})])
        .unwrap();
    Fixture {
        graph,
        workspace,
        project,
        actor,
        epic,
        registration,
        closure,
        guarded,
    }
}

impl Fixture {
    fn load(&self) -> Result<Registration, Refusal> {
        Registration::load(
            &self.graph.get(self.registration),
            self.workspace,
            self.actor,
            SERVICE,
            self.project,
        )
    }
    fn version(&self) -> String {
        self.graph.get(self.registration)["version"].to_string()
    }
    fn restore(&self, revision: i64) -> Restore {
        Restore {
            operation_id: Uuid::new_v4(),
            project_id: self.project,
            registration_id: self.registration,
            expected_version: self.version(),
            revision,
        }
    }
    fn plan_restore(&self, revision: i64) -> Planned {
        let reg = self.load()?;
        self.restore(revision).plan(
            &reg,
            &self.graph.get(self.epic),
            &self.graph.get(self.closure),
            &self.graph.revision(self.closure, revision),
        )
    }
    fn gates(&self, reg: &Registration) -> [Value; 3] {
        [reg.approval, reg.cleanup, reg.closure].map(|id| self.graph.context(id))
    }
    fn records(&self, reg: &Registration) -> BTreeMap<Uuid, Value> {
        reg.anchors()
            .into_iter()
            .map(|id| (id, self.graph.get(id)))
            .collect()
    }
    fn decide(&mut self, revision: i32, choice: Choice) -> Result<(), String> {
        let reg = self.load()?;
        let [a, c, g] = self.gates(&reg);
        let decide = Decide {
            operation_id: Uuid::new_v4(),
            project_id: self.project,
            registration_id: self.registration,
            expected_version: self.version(),
            revision,
            choice,
            vetted: REPOS
                .iter()
                .enumerate()
                .map(|(i, r)| (r.to_string(), sha(i as u8 + 1)))
                .collect(),
            evidence: artifact(0xd0),
        };
        assert!(decide.valid());
        let operations = decide.plan(&reg, &a, &c, &g)?;
        self.graph.apply(&operations)
    }
    fn evidence(&mut self, evidence: Evidence) -> Result<(), String> {
        assert!(evidence.valid());
        let reg = self.load()?;
        let [a, c, g] = self.gates(&reg);
        let operations = evidence.plan(&reg, [&a, &c, &g], &self.records(&reg))?;
        self.graph.apply(&operations)
    }
    fn base(&self, kind: EvidenceKind, proof: u8) -> Evidence {
        Evidence {
            operation_id: Uuid::new_v4(),
            project_id: self.project,
            registration_id: self.registration,
            expected_version: self.version(),
            kind,
            approval_revision: 2,
            evidence: artifact(proof),
            repository: None,
            pull_request: None,
            sha: None,
            supersedes_merge: None,
            passed: None,
            flag_absent: None,
            revisions: None,
            builds: None,
        }
    }
    fn merge(&self, repo: &str, commit: String, supersedes: Option<Uuid>) -> Evidence {
        Evidence {
            repository: Some(repo.into()),
            pull_request: Some(format!("https://github.com/{repo}/pull/7")),
            sha: Some(commit),
            supersedes_merge: supersedes,
            ..self.base(EvidenceKind::Merge, 0x11)
        }
    }
    fn ordinary(&self, repo: &str, commit: String, proof: u8, passed: bool) -> Evidence {
        Evidence {
            repository: Some(repo.into()),
            sha: Some(commit),
            passed: Some(passed),
            flag_absent: Some(true),
            ..self.base(EvidenceKind::Ordinary, proof)
        }
    }
    fn integrated(
        &self,
        merges: &BTreeMap<String, String>,
        builds: &BTreeMap<String, Artifact>,
    ) -> Evidence {
        Evidence {
            revisions: Some(merges.clone()),
            builds: Some(builds.clone()),
            passed: Some(true),
            ..self.base(EvidenceKind::Integrated, 0xee)
        }
    }
    fn close(&mut self) -> Result<(), String> {
        let reg = self.load()?;
        let [a, c, g] = self.gates(&reg);
        let close = Close {
            operation_id: Uuid::new_v4(),
            project_id: self.project,
            registration_id: self.registration,
            expected_version: self.version(),
        };
        assert!(close.valid());
        let operations = close.plan(
            &reg,
            &self.graph.get(self.epic),
            [&a, &c, &g],
            &self.records(&reg),
            &self.graph.tree(self.epic),
        )?;
        self.graph.apply(&operations)
    }
    fn state(&self) -> Value {
        self.graph.get(self.registration)["metadata"][META].clone()
    }
}

fn sha(seed: u8) -> String {
    format!("{seed:02x}").repeat(20)
}
fn artifact(seed: u8) -> Artifact {
    Artifact {
        artifact_id: Uuid::from_bytes([seed; 16]),
        sha256: format!("{seed:02x}").repeat(32),
    }
}

#[test]
fn restore_reuses_the_gate_identity_parent_and_verification() {
    let mut f = fixture();
    let before = f.state();
    let operations = f.plan_restore(f.guarded).unwrap();
    assert_eq!(operations[2]["op"], "restore_node");
    assert_eq!(operations[2]["id"], json!(f.closure));
    assert_eq!(operations[2]["include_lifecycle"], true);
    f.graph.apply(&operations).unwrap();
    let gate = f.graph.get(f.closure);
    assert_eq!(gate["deleted"], false);
    assert_eq!(gate["task_state"], "draft");
    assert_eq!(gate["verification"], "verified");
    assert_eq!(gate["last_verified_by"], SERVICE);
    assert_eq!(
        gate["metadata"][META],
        json!({"registration":f.registration,"role":"closure_gate"})
    );
    let parent = Uuid::new_v5(
        &f.registration,
        format!("parent-child/{}/{}", f.epic, f.closure).as_bytes(),
    );
    assert!(
        f.graph
            .live_edges(f.closure)
            .iter()
            .any(|e| e["id"] == json!(parent) && e["source"] == json!(f.epic))
    );
    // The registration and its protected state are unchanged.
    assert_eq!(f.state(), before);
    // A repeated restore finds a live gate: nothing to undelete.
    assert!(f.plan_restore(f.guarded).is_err());
}

#[test]
fn restore_refuses_other_owners_projects_revisions_and_stale_versions() {
    let f = fixture();
    let node = f.graph.get(f.registration);
    for (actor, project) in [(Uuid::new_v4(), f.project), (f.actor, Uuid::new_v4())] {
        assert!(Registration::load(&node, f.workspace, actor, SERVICE, project).is_err());
    }
    assert!(Registration::load(&node, Uuid::new_v4(), f.actor, SERVICE, f.project).is_err());
    assert!(Registration::load(&node, f.workspace, f.actor, "other-service", f.project).is_err());
    // A Graph-direct registration names a Graph actor instead of the AQ owner.
    let mut direct = node.clone();
    direct["metadata"][META]["spec"]
        .as_object_mut()
        .unwrap()
        .remove("owner_authority");
    assert!(Registration::load(&direct, f.workspace, f.actor, SERVICE, f.project).is_err());
    // The revisions before verification lack the guarded verification.
    for revision in 1..f.guarded {
        assert!(f.plan_restore(revision).is_err(), "revision {revision}");
    }
    // A different node's history is not the registered gate.
    let reg = f.load().unwrap();
    let epic = f.graph.get(f.epic);
    let other = f
        .graph
        .revision(f.epic, epic["content_revision"].as_i64().unwrap());
    let restore = f.restore(f.guarded);
    assert!(
        restore
            .plan(&reg, &epic, &f.graph.get(f.closure), &other)
            .is_err()
    );
    assert!(
        restore
            .plan(
                &reg,
                &epic,
                &f.graph.get(reg.cleanup),
                &f.graph.revision(f.closure, f.guarded)
            )
            .is_err()
    );
    // A batch planned against an older registration version is refused by CAS.
    let operations = f.plan_restore(f.guarded).unwrap();
    let mut moved = f.graph.clone();
    let current = moved.get(f.registration);
    moved
        .apply(&[
            json!({"op":"verify_node","id":f.registration,"expected_version":current["version"],
            "verification":"verified","evidence":[task_evidence(f.epic, "epic")]}),
        ])
        .unwrap();
    assert!(moved.apply(&operations).unwrap_err().starts_with("stale"));
}

#[test]
fn decisions_require_the_restored_gate() {
    let mut f = fixture();
    let error = f.decide(1, Choice::Approve).unwrap_err();
    assert!(error.contains("another owner or project"), "{error}");
    f.graph.apply(&f.plan_restore(f.guarded).unwrap()).unwrap();
    f.decide(1, Choice::Approve).unwrap();
}

#[test]
fn five_repository_evidence_closes_the_epic_once() {
    let mut f = fixture();
    f.graph.apply(&f.plan_restore(f.guarded).unwrap()).unwrap();
    assert!(f.close().unwrap_err().contains("approval is still pending"));
    f.decide(1, Choice::RequestChanges).unwrap();
    assert_eq!(
        f.graph.get(f.load().unwrap().cleanup)["task_state"],
        "draft"
    );
    assert!(
        f.decide(1, Choice::Approve).is_err(),
        "decision revisions advance"
    );
    f.decide(2, Choice::Approve).unwrap();
    let reg = f.load().unwrap();
    assert_eq!(f.graph.get(reg.approval)["task_state"], "completed");
    assert_eq!(f.graph.get(reg.cleanup)["task_state"], "active");
    assert!(
        f.decide(3, Choice::Approve).is_err(),
        "cleanup is released once"
    );

    let mut merges = BTreeMap::new();
    let mut builds = BTreeMap::new();
    for (i, repo) in REPOS.iter().enumerate() {
        let commit = sha(0x40 + i as u8);
        assert!(
            f.evidence(f.ordinary(repo, commit.clone(), 0x60, true))
                .is_err(),
            "a build needs its merge"
        );
        f.evidence(f.merge(repo, commit.clone(), None)).unwrap();
        merges.insert(repo.to_string(), commit);
        assert!(f.close().unwrap_err().contains("pending"));
    }
    assert!(
        f.evidence(f.merge("Acme/other", sha(0x99), None)).is_err(),
        "unregistered repository"
    );
    // The first server build fails, then passes after a repair merge.
    let first = REPOS[0];
    f.evidence(f.ordinary(first, merges[first].clone(), 0x50, false))
        .unwrap();
    let current = id_of(&f.state()["merges"][first]).unwrap();
    assert!(
        f.evidence(f.merge(first, sha(0x90), None)).is_err(),
        "a repair must supersede"
    );
    f.evidence(f.merge(first, sha(0x90), Some(current)))
        .unwrap();
    merges.insert(first.to_string(), sha(0x90));
    assert!(f.state()["ordinary"].get(first).is_none());
    for (i, repo) in REPOS.iter().enumerate() {
        let proof = artifact(0x60 + i as u8);
        let mut build = f.ordinary(repo, merges[*repo].clone(), 0x60 + i as u8, true);
        assert!(
            f.evidence(Evidence {
                sha: Some(sha(0x77)),
                ..build.clone()
            })
            .is_err(),
            "other SHA"
        );
        build.operation_id = Uuid::new_v4();
        f.evidence(build).unwrap();
        builds.insert(repo.to_string(), proof);
    }
    assert!(f.close().unwrap_err().contains("integrated"));
    let mut partial = builds.clone();
    partial.remove(REPOS[4]);
    assert!(
        f.evidence(f.integrated(&merges, &partial)).is_err(),
        "every exact build"
    );
    let mut stale = merges.clone();
    stale.insert(first.to_string(), sha(0x40));
    assert!(
        f.evidence(f.integrated(&stale, &builds)).is_err(),
        "the repaired merge"
    );
    f.evidence(f.integrated(&merges, &builds)).unwrap();
    // The cleanup task completes only through ordinary delivery.
    assert!(f.close().unwrap_err().contains("normal delivery"));
    let cleanup = f.graph.get(reg.cleanup);
    f.graph
        .apply(&[json!({"op":"update_node","id":reg.cleanup,"expected_version":cleanup["version"],"patch":{"task_state":"completed"}})])
        .unwrap();
    f.close().unwrap();
    let epic = f.graph.get(f.epic);
    let gate = f.graph.get(f.closure);
    assert_eq!(epic["task_state"], "completed");
    assert_eq!(gate["task_state"], "completed");
    assert_eq!(gate["verification"], "verified");
    let state = f.state();
    let receipt = Uuid::new_v5(&f.registration, b"close");
    assert_eq!(state["closed"], json!(receipt));
    assert_eq!(gate["metadata"][META]["receipt"], json!(receipt));
    let closure = &f.graph.get(receipt)["metadata"][META];
    assert_eq!(closure["approval_revision"], 2);
    assert_eq!(closure["merges"].as_object().unwrap().len(), 5);
    assert!(f.close().is_err(), "closed once");
    assert!(f.evidence(f.merge(REPOS[1], sha(0x91), None)).is_err());
}

#[test]
fn closure_refuses_an_open_sibling_task() {
    let mut f = fixture();
    f.graph.apply(&f.plan_restore(f.guarded).unwrap()).unwrap();
    f.decide(1, Choice::Approve).unwrap();
    let sibling = task(&mut f.graph, f.project, f.actor, "active");
    f.graph
        .apply(&[json!({"op":"add_edge","id":Uuid::new_v4(),"edge_type":"parent-child","source":f.epic,"target":sibling})])
        .unwrap();
    let approval = |e: Evidence| Evidence {
        approval_revision: 1,
        ..e
    };
    let mut merges = BTreeMap::new();
    let mut builds = BTreeMap::new();
    for (i, repo) in REPOS.iter().enumerate() {
        f.evidence(approval(f.merge(repo, sha(0x40 + i as u8), None)))
            .unwrap();
        f.evidence(approval(f.ordinary(repo, sha(0x40 + i as u8), 0x60, true)))
            .unwrap();
        merges.insert(repo.to_string(), sha(0x40 + i as u8));
        builds.insert(repo.to_string(), artifact(0x60));
    }
    f.evidence(approval(f.integrated(&merges, &builds)))
        .unwrap();
    let reg = f.load().unwrap();
    let cleanup = f.graph.get(reg.cleanup);
    f.graph
        .apply(&[json!({"op":"update_node","id":reg.cleanup,"expected_version":cleanup["version"],"patch":{"task_state":"completed"}})])
        .unwrap();
    assert!(f.close().unwrap_err().contains("still open"));
}

#[test]
fn payload_bounds() {
    let f = fixture();
    let mut restore = f.restore(0);
    assert!(!restore.valid());
    restore.revision = f.guarded;
    assert!(restore.valid());
    restore.registration_id = Uuid::nil();
    assert!(!restore.valid());
    let merge = f.merge(REPOS[0], sha(1), None);
    assert!(merge.valid());
    for bad in [
        Evidence {
            sha: Some("abc".into()),
            ..merge.clone()
        },
        Evidence {
            sha: Some("0".repeat(40)),
            ..merge.clone()
        },
        Evidence {
            pull_request: Some("http://github.com/x".into()),
            ..merge.clone()
        },
        Evidence {
            pull_request: Some("https://user@github.com/x".into()),
            ..merge.clone()
        },
        Evidence {
            passed: Some(true),
            ..merge.clone()
        },
        Evidence {
            evidence: Artifact {
                sha256: "x".into(),
                ..artifact(1)
            },
            ..merge.clone()
        },
    ] {
        assert!(!bad.valid());
    }
    assert!(
        !Evidence {
            flag_absent: None,
            ..f.ordinary(REPOS[0], sha(1), 1, true)
        }
        .valid()
    );
    assert!(!f.integrated(&BTreeMap::new(), &BTreeMap::new()).valid());
}

/// The model served as Graph reads, the way the server's runtime serves them.
struct Served<'a>(&'a Graph, Uuid);
impl super::super::dispatch::Reads for Served<'_> {
    type Error = String;
    async fn get(&self, path: String, _: &'static str) -> Result<Value, String> {
        let prefix = format!("/v1/workspaces/{}/nodes/", self.1);
        let rest = path.strip_prefix(&prefix).ok_or("another workspace")?;
        let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let id: Uuid = id.parse().map_err(|_| "malformed path")?;
        if !self.0.nodes.contains_key(&id) {
            return Err("not found".into());
        }
        let mut raw = match tail {
            "" => self.0.get(id),
            "context" => self.0.context(id),
            "tree?depth=1&limit=200" => self.0.tree(id),
            tail => {
                let n = tail.strip_prefix("revisions/").and_then(|n| n.parse().ok());
                let n: i64 = n.ok_or("malformed path")?;
                self.0.revisions.get(&(id, n)).cloned().ok_or("not found")?
            }
        };
        let node = if raw.get("node").is_some() {
            &mut raw["node"]
        } else {
            &mut raw
        };
        node["workspace_id"] = json!(self.1);
        Ok(raw)
    }
    fn fail(&self, refusal: Option<Refusal>) -> String {
        refusal.unwrap_or("unavailable").into()
    }
}

#[tokio::test]
async fn dispatch_reads_only_what_the_registration_names() {
    use super::super::dispatch::{Action, plan};
    let mut f = fixture();
    // Only registration reads a journal table; these actions never touch it.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .unwrap();
    let at = (f.workspace, f.actor, f.project, SERVICE);
    let decide = Decide {
        operation_id: Uuid::new_v4(),
        project_id: f.project,
        registration_id: f.registration,
        expected_version: f.version(),
        revision: 1,
        choice: Choice::Approve,
        vetted: REPOS.iter().map(|r| (r.to_string(), sha(1))).collect(),
        evidence: artifact(0xd0),
    };
    let restore = f.restore(f.guarded);
    let subject = f.graph.get(f.registration);
    let operations = {
        let r = Served(&f.graph, f.workspace);
        let refused = plan(&r, &pool, at, &subject, Action::Decide(&decide)).await;
        assert_eq!(refused.unwrap_err(), "a registered record is deleted");
        let other = (f.workspace, Uuid::new_v4(), f.project, SERVICE);
        let refused = plan(&r, &pool, other, &subject, Action::Restore(&restore)).await;
        assert_eq!(
            refused.unwrap_err(),
            "registration belongs to another owner"
        );
        let elsewhere = Served(&f.graph, Uuid::new_v4());
        let refused = plan(&elsewhere, &pool, at, &subject, Action::Restore(&restore)).await;
        assert!(refused.is_err());
        plan(&r, &pool, at, &subject, Action::Restore(&restore))
            .await
            .unwrap()
    };
    assert_eq!(operations, f.plan_restore(f.guarded).unwrap());
    f.graph.apply(&operations).unwrap();
    let subject = f.graph.get(f.registration);
    let r = Served(&f.graph, f.workspace);
    let decided = plan(&r, &pool, at, &subject, Action::Decide(&decide))
        .await
        .unwrap();
    f.graph.apply(&decided).unwrap();
    let close = Close {
        operation_id: Uuid::new_v4(),
        project_id: f.project,
        registration_id: f.registration,
        expected_version: f.version(),
    };
    let subject = f.graph.get(f.registration);
    let r = Served(&f.graph, f.workspace);
    let pending = plan(&r, &pool, at, &subject, Action::Close(&close)).await;
    assert_eq!(pending.unwrap_err(), "a cleanup merge is pending");
}
