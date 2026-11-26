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

mod graph;
use graph::Graph;

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
            super::super::Scope {
                workspace,
                actor,
                project,
                service: SERVICE,
            },
            &graph.get(epic),
            [&graph.context(approval), &graph.context(cleanup)],
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
    /// The same identities over an empty model.
    fn clone_ids(&self) -> Fixture {
        Fixture {
            graph: Graph::default(),
            ..*self
        }
    }
    fn scope(&self) -> Scope<'static> {
        Scope {
            workspace: self.workspace,
            actor: self.actor,
            service: SERVICE,
            project: self.project,
        }
    }
    fn load(&self) -> Result<Registration, Refusal> {
        Registration::load(&self.graph.get(self.registration), self.scope())
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
            GateRevision {
                tombstone: &self.graph.get(self.closure),
                snapshot: &self.graph.revision(self.closure, revision),
            },
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
    fn decision(&self, revision: i32, choice: Choice) -> Decide {
        Decide {
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
        }
    }
    fn plan_decision(&self, decide: &Decide, journaled: bool) -> Planned {
        assert!(decide.valid());
        let reg = self.load()?;
        let [a, c, g] = self.gates(&reg);
        decide.plan(&reg, [&a, &c, &g], journaled)
    }
    fn decide(&mut self, revision: i32, choice: Choice) -> Result<(), String> {
        let operations = self.plan_decision(&self.decision(revision, choice), false)?;
        self.graph.apply(&operations)
    }
    /// One ordinary Graph task edit outside the retirement workflow.
    fn edit(&mut self, id: Uuid, patch: Value) {
        let node = self.graph.get(id);
        self.graph
            .apply(&[json!({"op":"update_node","id":id,"expected_version":node["version"],"patch":patch})])
            .unwrap();
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
    fn ordinary(&self, repo: &str, commit: String, (proof, passed): (u8, bool)) -> Evidence {
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
            ClosureReads {
                epic: &self.graph.get(self.epic),
                gates: [&a, &c, &g],
                records: &self.records(&reg),
                tree: &self.graph.tree(self.epic),
            },
        )?;
        self.graph.apply(&operations)
    }
    fn state(&self) -> Value {
        self.graph.get(self.registration)["metadata"][META].clone()
    }
}

#[test]
fn evidence_anchors_hash_nested_keys_in_canonical_order() {
    let event = json!({"z":"approved","a":[{"z":"é","a":1},true],"n":null});
    assert_eq!(
        digest(&event),
        "eb85894d02f588cbe361f3dc0ed87cf967df9abcaa24e7ae250557784feba81f"
    );
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
        assert!(
            Registration::load(
                &node,
                Scope {
                    actor,
                    project,
                    ..f.scope()
                }
            )
            .is_err()
        );
    }
    assert!(
        Registration::load(
            &node,
            Scope {
                workspace: Uuid::new_v4(),
                ..f.scope()
            }
        )
        .is_err()
    );
    assert!(
        Registration::load(
            &node,
            Scope {
                service: "other-service",
                ..f.scope()
            }
        )
        .is_err()
    );
    // A Graph-direct registration names a Graph actor instead of the AQ owner.
    let mut direct = node.clone();
    direct["metadata"][META]["spec"]
        .as_object_mut()
        .unwrap()
        .remove("owner_authority");
    assert!(Registration::load(&direct, f.scope()).is_err());
}

#[test]
fn restore_refuses_unverified_or_unrelated_gate_revisions() {
    let f = fixture();
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
            .plan(
                &reg,
                &epic,
                GateRevision {
                    tombstone: &f.graph.get(f.closure),
                    snapshot: &other
                }
            )
            .is_err()
    );
    assert!(
        restore
            .plan(
                &reg,
                &epic,
                GateRevision {
                    tombstone: &f.graph.get(reg.cleanup),
                    snapshot: &f.graph.revision(f.closure, f.guarded)
                }
            )
            .is_err()
    );
}

#[test]
fn restore_keeps_a_compare_and_swap_guard_on_the_registration() {
    let f = fixture();
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

    record_five_repositories(&mut f);
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

fn record_five_repositories(f: &mut Fixture) {
    let mut merges = BTreeMap::new();
    let mut builds = BTreeMap::new();
    for (i, repo) in REPOS.iter().enumerate() {
        let commit = sha(0x40 + i as u8);
        assert!(
            f.evidence(f.ordinary(repo, commit.clone(), (0x60, true)))
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
    let first = REPOS[0];
    repair_server_merge(f, &mut merges);
    for (i, repo) in REPOS.iter().enumerate() {
        let proof = artifact(0x60 + i as u8);
        let mut build = f.ordinary(repo, merges[*repo].clone(), (0x60 + i as u8, true));
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
}

fn repair_server_merge(f: &mut Fixture, merges: &mut BTreeMap<String, String>) {
    // The first server build fails, then passes after a repair merge.
    let first = REPOS[0];
    f.evidence(f.ordinary(first, merges[first].clone(), (0x50, false)))
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
        f.evidence(approval(f.ordinary(
            repo,
            sha(0x40 + i as u8),
            (0x60, true),
        )))
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

/// The registered Epic 0 shape: the closure gate restored, but the approval
/// task already completed as ordinary work and the cleanup task released and
/// worked on, with no decision on the registration.
fn released_before_decision() -> Fixture {
    let mut f = fixture();
    f.graph.apply(&f.plan_restore(f.guarded).unwrap()).unwrap();
    let reg = f.load().unwrap();
    f.edit(reg.approval, json!({"task_state":"active"}));
    f.edit(reg.approval, json!({"task_state":"completed"}));
    f.edit(reg.cleanup, json!({"task_state":"active"}));
    f.edit(
        reg.cleanup,
        json!({"title":"Remove the delivery_playbook flag"}),
    );
    assert!(f.state().get("decision").is_none());
    f
}

/// Merges, passing flag-free ordinary builds and the combined check for
/// every registered repository, then the cleanup's ordinary completion.
fn settle_cleanup(f: &mut Fixture, approval_revision: i32) {
    let at = |e: Evidence| Evidence {
        approval_revision,
        ..e
    };
    let mut merges = BTreeMap::new();
    let mut builds = BTreeMap::new();
    for (i, repo) in REPOS.iter().enumerate() {
        let commit = sha(0x40 + i as u8);
        f.evidence(at(f.merge(repo, commit.clone(), None))).unwrap();
        f.evidence(at(f.ordinary(repo, commit.clone(), (0x60 + i as u8, true))))
            .unwrap();
        merges.insert(repo.to_string(), commit);
        builds.insert(repo.to_string(), artifact(0x60 + i as u8));
    }
    f.evidence(at(f.integrated(&merges, &builds))).unwrap();
    let cleanup = f.load().unwrap().cleanup;
    if f.graph.get(cleanup)["task_state"] != "completed" {
        f.edit(cleanup, json!({"task_state":"completed"}));
    }
}

#[test]
fn a_release_before_any_decision_is_reconciled_by_an_explicit_owner_decision() {
    let mut f = released_before_decision();
    let reg = f.load().unwrap();
    let (approval, cleanup) = (f.graph.get(reg.approval), f.graph.get(reg.cleanup));
    // Task state is never approval: evidence and closure still need a decision.
    assert!(
        f.evidence(Evidence {
            approval_revision: 1,
            ..f.merge(REPOS[0], sha(0x40), None)
        })
        .unwrap_err()
        .contains("approval is still pending")
    );
    assert!(f.close().unwrap_err().contains("approval is still pending"));
    let approve = f.decision(1, Choice::Approve);
    let refused = |f: &Fixture, decide: &Decide, journaled| {
        f.plan_decision(decide, journaled).unwrap_err().to_string()
    };
    assert!(refused(&f, &approve, false).contains("this owner's AQ registration"));
    let changes = f.decision(1, Choice::RequestChanges);
    assert!(refused(&f, &changes, true).contains("returned for changes"));
    assert!(refused(&f, &f.decision(2, Choice::Approve), true).contains("follow"));
    let mut partial = approve.clone();
    partial.vetted.remove(REPOS[4]);
    assert!(refused(&f, &partial, true).contains("registered repositories"));
    let mut extra = approve.clone();
    extra.vetted.insert("Acme/other".into(), sha(9));
    assert!(refused(&f, &extra, true).contains("registered repositories"));

    // Version races: the planned batch is fenced on every task it relies on.
    let operations = f.plan_decision(&approve, true).unwrap();
    for moved in [reg.approval, reg.cleanup, reg.closure, f.registration] {
        let mut raced = Fixture {
            graph: f.graph.clone(),
            ..f.clone_ids()
        };
        raced.edit(moved, json!({"title":"Concurrent edit"}));
        let error = raced.graph.apply(&operations).unwrap_err();
        assert!(error.starts_with("stale"), "{moved}: {error}");
    }

    f.graph.apply(&operations).unwrap();
    // The original work is untouched and nothing is released again.
    assert_eq!(f.graph.get(reg.approval), approval);
    assert_eq!(f.graph.get(reg.cleanup), cleanup);
    let anchor = Uuid::new_v5(&f.registration, b"decision/1");
    let state = f.state();
    assert_eq!(state["approved"], json!(anchor));
    assert_eq!(state["reconciled"], json!(anchor));
    assert_eq!(state["decision_revision"], 1);
    let record = f.graph.get(anchor);
    assert_eq!(record["verification"], "verified");
    let event = &record["metadata"][META];
    assert_eq!(event["choice"], "approve");
    assert_eq!(event["vetted"].as_object().unwrap().len(), REPOS.len());
    assert_eq!(
        event["released_before_decision"]["cleanup"]["version"],
        cleanup["version"]
    );
    assert!(f.graph.get(f.registration)["verification"] == "verified");
    // Replay with a fresh operation, or a later decision, is refused.
    assert!(
        f.plan_decision(&f.decision(1, Choice::Approve), true)
            .is_err()
    );
    assert!(
        f.plan_decision(&f.decision(2, Choice::Approve), true)
            .is_err()
    );

    settle_cleanup(&mut f, 1);
    f.close().unwrap();
    assert_eq!(f.graph.get(f.epic)["task_state"], "completed");
    assert_eq!(f.graph.get(f.closure)["task_state"], "completed");
    assert!(f.close().is_err(), "closed once");
}

#[test]
fn reconciliation_refuses_tasks_that_are_not_the_registered_release() {
    let approve = |f: &Fixture| f.plan_decision(&f.decision(1, Choice::Approve), true);
    // The closure gate must be restored first.
    let mut f = fixture();
    let reg = f.load().unwrap();
    f.edit(reg.approval, json!({"task_state":"completed"}));
    assert!(
        approve(&f)
            .unwrap_err()
            .contains("another owner or project")
    );
    // An approval completed by this workflow carries its decision marker.
    let mut f = released_before_decision();
    let reg = f.load().unwrap();
    let mut metadata = f.graph.get(reg.approval)["metadata"].clone();
    metadata[META] = json!({"registration":f.registration,"role":"approval"});
    f.edit(reg.approval, json!({"metadata":metadata}));
    assert!(approve(&f).unwrap_err().contains("outside a decision"));
    // The cleanup must still be the registration's own task.
    let mut f = released_before_decision();
    let reg = f.load().unwrap();
    let mut metadata = f.graph.get(reg.cleanup)["metadata"].clone();
    metadata.as_object_mut().unwrap().remove(META);
    f.edit(reg.cleanup, json!({"metadata":metadata}));
    assert!(approve(&f).is_err());
    // A canceled cleanup is not released work to preserve.
    let mut f = released_before_decision();
    let reg = f.load().unwrap();
    f.edit(reg.cleanup, json!({"task_state":"canceled"}));
    assert!(
        approve(&f)
            .unwrap_err()
            .contains("released before a decision")
    );
    // Another owner's or project's task.
    let mut f = released_before_decision();
    let reg = f.load().unwrap();
    let mut metadata = f.graph.get(reg.approval)["metadata"].clone();
    metadata["aq:human_actor_id"] = json!(Uuid::new_v4());
    f.edit(reg.approval, json!({"metadata":metadata}));
    assert!(
        approve(&f)
            .unwrap_err()
            .contains("another owner or project")
    );
    // A recorded request for changes followed by an outside completion.
    let mut f = fixture();
    f.graph.apply(&f.plan_restore(f.guarded).unwrap()).unwrap();
    f.decide(1, Choice::RequestChanges).unwrap();
    let reg = f.load().unwrap();
    f.edit(reg.approval, json!({"task_state":"completed"}));
    let error = f
        .plan_decision(&f.decision(2, Choice::Approve), true)
        .unwrap_err();
    assert!(error.contains("already recorded"), "{error}");
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
            ..f.ordinary(REPOS[0], sha(1), (1, true))
        }
        .valid()
    );
    assert!(!f.integrated(&BTreeMap::new(), &BTreeMap::new()).valid());
}

mod dispatch;
