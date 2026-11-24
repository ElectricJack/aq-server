use super::*;
use std::collections::BTreeMap;

fn actions() -> Vec<ActionSpec> {
    vec![
        ActionSpec {
            kind: "task.create",
            grant: "task.create.subtask",
            target: TargetRule::ParentTask {
                field: "parent_id",
                unparented: "task.create.project",
            },
        },
        ActionSpec {
            kind: "task.graph.create",
            grant: "task.create.subtask",
            target: TargetRule::ParentTask {
                field: "parent_id",
                unparented: "task.create.project",
            },
        },
        ActionSpec {
            kind: "task.comment.add",
            grant: "comment.write",
            target: TargetRule::Subject,
        },
        ActionSpec {
            kind: "decision.resolve",
            grant: "decision.resolve",
            target: TargetRule::Subject,
        },
    ]
}

#[test]
fn registry_merges_actions_and_refuses_disagreement() {
    let r = Registry::new(&actions()).unwrap();
    let add = r.get("task.comment.add").unwrap();
    assert_eq!(add.owner, Some(Route::action()));
    assert_eq!(add.agent.unwrap().path, "/v1/agent/comments");
    let resolve = r.get("decision.resolve").unwrap();
    assert!(resolve.owner_only() && resolve.agent.is_none());
    assert!(!add.owner_only());
    assert_eq!(add.cli(), ["task", "comment", "add"]);
    let mut wrong = actions();
    wrong[2].grant = "comment.read";
    assert!(Registry::new(&wrong).is_err());
    // A catalog command that names the action union must be a registered action.
    assert!(Registry::new(&actions()[2..]).is_err());
}

fn request(params: Value) -> CommandRequest {
    CommandRequest {
        params: params.as_object().unwrap().clone(),
        ..Default::default()
    }
}

#[test]
fn forwarding_query_routes_validates_path_params() {
    let r = Registry::new(&actions()).unwrap();
    let show = r.get("task.show").unwrap();
    let p = Uuid::from_u128(2);
    // A declared query param follows the path params (queries.rs).
    let record = r.get("knowledge.show").unwrap();
    let f = record
        .forward(
            record.owner.as_ref().unwrap(),
            &request(json!({"project_id":p,"node_id":"t-1","revision":3})),
            None,
        )
        .unwrap();
    assert_eq!(
        f.uri,
        format!("/api/human/projects/{p}/knowledge/t-1?revision=3")
    );
    assert!(f.body.is_none());
    let bad = show.forward(
        show.owner.as_ref().unwrap(),
        &request(json!({"project_id":"../x","task_id":"t"})),
        None,
    );
    assert!(bad.is_err(), "path params are plain identifiers");
}

#[test]
fn forwarding_versioned_and_graph_requests_includes_protocol_metadata() {
    let r = Registry::new(&actions()).unwrap();
    let rid = Uuid::from_u128(1);
    let binding = json!({"attempt_id":"a"});
    let ask = r.get("question.ask").unwrap();
    let f = ask
        .forward(
            ask.agent.as_ref().unwrap(),
            &request(json!({"operation_id":"o","question":"q?"})),
            None,
        )
        .unwrap();
    assert_eq!(
        f.body.unwrap(),
        json!({"api_version":1,"operation_id":"o","question":"q?"})
    );
    let search = r.get("knowledge.search").unwrap();
    let f = search
        .forward(
            search.agent.as_ref().unwrap(),
            &request(json!({"text":"release"})),
            Some(AgentRequest {
                binding: &binding,
                request_id: rid,
            }),
        )
        .unwrap();
    assert_eq!(
        f.body.unwrap(),
        json!({"api_version":1,"request_id":rid,"binding":binding,"query":{"kind":"search","text":"release","kind_filter":null,"limit":20,"cursor":null}})
    );
}

#[test]
fn forwarding_an_action_preserves_identity_and_requires_a_subject() {
    let r = Registry::new(&actions()).unwrap();
    let create = r.get("task.create").unwrap();
    let op = Uuid::from_u128(3);
    let mut request = request(json!({"title":"x"}));
    request.operation_id = Some(op);
    request.subject = Some(json!({"type":"project","id":"p","revision":"4"}));
    let f = create
        .forward(create.owner.as_ref().unwrap(), &request, None)
        .unwrap();
    assert_eq!((f.method, f.uri.as_str()), ("POST", "/api/v1/actions"));
    let body = f.body.unwrap();
    assert_eq!(
        (body["kind"].clone(), body["operation_id"].clone()),
        (json!("task.create"), json!(op))
    );
    // The payload always carries the operation ID.
    assert_eq!(body["payload"], json!({"title":"x","operation_id":op}));
    request.subject = None;
    assert!(
        create
            .forward(create.owner.as_ref().unwrap(), &request, None)
            .is_err()
    );
}

#[test]
fn document_lists_every_command_with_digests() {
    let r = Registry::new(&actions()).unwrap();
    let schemas: BTreeMap<String, Value> = actions()
        .iter()
        .map(|a| {
            (
                a.kind.to_string(),
                json!({"type":"object","additionalProperties":false}),
            )
        })
        .collect();
    let doc = document(&r, &schemas);
    let commands = doc["commands"].as_array().unwrap();
    assert_eq!(commands.len(), r.commands().len());
    let whoami = commands.iter().find(|c| c["name"] == "whoami").unwrap();
    assert_eq!(whoami["grant"], Value::Null);
    assert!(whoami["principals"]["agent"].is_object());
    for c in commands {
        for (_, p) in c["principals"].as_object().unwrap() {
            assert!(p["params_digest"].as_str().unwrap().starts_with("sha256:"));
        }
    }
}

#[test]
fn reviewed_routes_parse_and_are_not_also_commands() {
    let r = Registry::new(&actions()).unwrap();
    let reviewed = routes::reviewed();
    assert!(!reviewed.is_empty());
    let mut seen = std::collections::BTreeSet::new();
    for route in &reviewed {
        assert!(seen.insert(route.path.clone()), "duplicate {}", route.path);
        assert!(
            !route.note.trim().is_empty(),
            "{} needs a review note",
            route.path
        );
        if route.class != routes::Class::Command {
            let served = r.commands().iter().any(|c| {
                [&c.owner, &c.supervisor, &c.agent]
                    .into_iter()
                    .flatten()
                    .any(|x| x.path == route.path && x.body != Body::Local)
            });
            assert!(
                !served,
                "{} is a command route; drop it from routes.json",
                route.path
            );
        }
    }
}

struct ScopedTaskReads {
    registry: Registry,
    project: Uuid,
    own: Uuid,
    grants: aq_agent_grants::GrantSet,
}

impl ScopedTaskReads {
    fn new() -> Self {
        use aq_agent_grants::{Grant, GrantSet, Scope};
        Self {
            registry: Registry::new(&actions()).unwrap(),
            project: Uuid::from_u128(7),
            own: Uuid::from_u128(8),
            grants: GrantSet::new(vec![
                Grant::new("task.read", Scope::TaskSubtree),
                Grant::new("comment.write", Scope::TaskSubtree),
            ])
            .unwrap(),
        }
    }
    fn run(
        &self,
        name: &str,
        params: Value,
        relation: aq_agent_grants::Relation,
    ) -> (
        aq_agent_grants::Decision,
        aq_agent_grants::Target,
        Result<Value, BuildError>,
    ) {
        let spec = self.registry.get(name).unwrap();
        let mut request = request(params);
        let (decision, target) = authorize(
            spec,
            &mut request.params,
            &self.grants,
            AgentScope {
                holder: aq_agent_grants::Holder {
                    project: self.project,
                },
                own_task: self.own,
                relation,
            },
        );
        let binding = json!({"attempt_id":"a"});
        let forwarded = spec.forward(
            spec.agent.as_ref().unwrap(),
            &request,
            Some(AgentRequest {
                binding: &binding,
                request_id: Uuid::from_u128(1),
            }),
        );
        (
            decision,
            target,
            forwarded.map(|f| f.body.unwrap()["query"].clone()),
        )
    }
}

#[test]
fn agent_task_reads_are_scoped_graph_reads_of_the_named_task() {
    use aq_agent_grants::{Decision, Relation, Target};
    let fixture = ScopedTaskReads::new();
    let (project, own, other) = (fixture.project, fixture.own, Uuid::from_u128(9));
    // Omitted: the attempt's own task, sent as the graph node.
    let (d, t, q) = fixture.run("task.show", json!({}), Relation::Other);
    assert!(matches!(d, Decision::Allow(_)));
    assert_eq!(
        t,
        Target::Task {
            project,
            relation: Relation::Own
        }
    );
    assert_eq!(q.unwrap(), json!({"kind":"get","node_id":own}));
    // Another task: its resolved relation decides the scope.
    let (d, _, _) = fixture.run("task.show", json!({"task_id":other}), Relation::Other);
    assert!(matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::ScopeDenied));
    let (d, _, q) = fixture.run(
        "task.children",
        json!({"task_id":other,"project_id":project}),
        Relation::Descendant(1),
    );
    assert!(matches!(d, Decision::Allow(_)));
    assert_eq!(
        q.unwrap(),
        json!({"kind":"relationships","node_id":other,"direction":"outgoing","edge_types":["parent-child"],"limit":64,"cursor":null})
    );
    let (_, _, q) = fixture.run("task.history", json!({"limit":5}), Relation::Other);
    assert_eq!(
        q.unwrap(),
        json!({"kind":"history","node_id":own,"limit":5,"cursor":null})
    );
}

#[test]
fn task_and_comment_scopes_refuse_unrelated_targets() {
    use aq_agent_grants::{Decision, Relation, Target};
    let fixture = ScopedTaskReads::new();
    let rid = Uuid::from_u128(1);
    // Another project is outside every agent scope but an owner override.
    let (d, t, _) = fixture.run(
        "task.show",
        json!({"project_id":Uuid::from_u128(5)}),
        Relation::Own,
    );
    assert!(matches!(d, Decision::Deny(_)));
    assert_eq!(t, Target::Project(Uuid::from_u128(5)));
    // A comment the caller did not write resolves outside every scope.
    let c = json!({"comment_id":Uuid::from_u128(4),"expected_revision":"1","operation_id":rid});
    let (d, _, _) = fixture.run("task.comment.delete", c.clone(), Relation::Other);
    assert!(matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::ScopeDenied));
    let (d, _, _) = fixture.run("task.comment.delete", c, Relation::Own);
    assert!(matches!(d, Decision::Allow(_)));
    let (d, _, _) = fixture.run(
        "task.comment.edit",
        json!({"comment_id":"x"}),
        Relation::Own,
    );
    assert!(
        matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::InvalidRequest)
    );
}

#[test]
fn discovery_agrees_with_task_grants() {
    let fixture = ScopedTaskReads::new();
    let r = fixture.registry;
    let grants = fixture.grants;
    // Discovery agrees with the check.
    let names = r.agent_commands(&grants);
    for n in [
        "task.show",
        "task.children",
        "task.history",
        "task.comment.edit",
    ] {
        assert!(names.contains(&n.to_string()), "{n}");
    }
    assert!(!names.contains(&"comment.list".to_string()));
    assert_eq!(
        available(
            r.get("task.show").unwrap(),
            Some(Principal::Agent),
            Some(&grants)
        ),
        Ok(Some("task:subtree".into()))
    );
    assert_eq!(
        available(
            r.get("task.list").unwrap(),
            Some(Principal::Agent),
            Some(&grants)
        ),
        Err(aq_agent_grants::RefusalCode::NotAvailable)
    );
}

#[test]
fn a_task_read_must_have_read_a_task() {
    let r = Registry::new(&actions()).unwrap();
    let show = r.get("task.show").unwrap();
    let node = |kind: &str| json!({"result":{"kind":"get","node":{"kind":kind}}});
    assert!(agent_result_ok(show, &node("task")));
    assert!(!agent_result_ok(show, &node("knowledge")));
    let history = r.get("task.history").unwrap();
    let items =
        json!({"result":{"kind":"history","items":[{"node":null},{"node":{"kind":"knowledge"}}]}});
    assert!(!agent_result_ok(history, &items));
    // Other graph reads are the knowledge grant's and pass unchanged.
    assert!(agent_result_ok(
        r.get("knowledge.show").unwrap(),
        &node("knowledge")
    ));
}

/// Every GET command route, with its command name.
fn query_routes() -> Vec<(&'static str, Route)> {
    let r = Registry::new(&actions()).unwrap();
    r.commands()
        .iter()
        .flat_map(|c| {
            [c.owner, c.supervisor, c.agent]
                .into_iter()
                .flatten()
                .filter(|route| route.body == Body::Query)
                .map(|route| (c.name, route))
        })
        .collect()
}

#[test]
fn every_get_route_declares_its_query_and_publishes_a_closed_schema() {
    let routes = query_routes();
    assert!(routes.len() >= 60);
    for (name, route) in routes {
        assert_eq!(route.params, Params::Path, "{name}");
        let declared = route
            .query()
            .unwrap_or_else(|| panic!("{name}: declare the query of {} in queries.rs", route.path));
        let schema = params_schema(&route, &BTreeMap::new(), name);
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let path: Vec<&str> = route.segments().map(|(_, p)| p).collect();
        let mut seen = std::collections::BTreeSet::new();
        for q in declared {
            let s: Value = serde_json::from_str(q.schema)
                .unwrap_or_else(|_| panic!("{name}: {} schema is not JSON", q.name));
            assert!(
                s.get("type").is_some() || s.get("enum").is_some(),
                "{name}: {}",
                q.name
            );
            assert!(
                seen.insert(q.name) && !path.contains(&q.name),
                "{name}: {}",
                q.name
            );
            assert_eq!(schema["properties"][q.name], s, "{name}: {}", q.name);
            let required = schema["required"].as_array().unwrap();
            assert_eq!(
                required.contains(&json!(q.name)),
                q.required,
                "{name}: {}",
                q.name
            );
        }
        assert_eq!(
            schema["properties"].as_object().unwrap().len(),
            path.len() + declared.len(),
            "{name}"
        );
    }
}

#[test]
fn supervisor_reads_declare_the_owner_query() {
    let r = Registry::new(&actions()).unwrap();
    for c in r.commands() {
        let (Some(owner), Some(supervisor)) = (c.owner, c.supervisor) else {
            continue;
        };
        let both_queries = (owner.body, supervisor.body) == (Body::Query, Body::Query);
        if both_queries && supervisor.path.ends_with(owner.path) {
            assert_eq!(owner.query(), supervisor.query(), "{}", c.name);
        }
    }
}

#[test]
fn a_supervisor_reads_hold_evidence_through_the_read_nest() {
    let r = Registry::new(&actions()).unwrap();
    for (name, path) in [
        ("result.list", "/v1/supervisor/read/api/human/results"),
        (
            "result.show",
            "/v1/supervisor/read/api/human/results/{id}/revisions/{revision}",
        ),
        (
            "result.detail",
            "/v1/supervisor/read/api/v1/results/{id}/revisions/{revision}",
        ),
        (
            "attempt.show",
            "/v1/supervisor/read/api/execution/attempts/{id}",
        ),
    ] {
        let c = r.get(name).unwrap();
        let (owner, supervisor) = (c.owner.unwrap(), c.supervisor.unwrap());
        assert_eq!(supervisor.path, path, "{name}");
        assert_eq!(supervisor.method, "GET", "{name}");
        assert_eq!(supervisor.rename, owner.rename, "{name}");
    }
}

#[test]
fn forward_refuses_an_undeclared_query_param() {
    let r = Registry::new(&actions()).unwrap();
    let req = |params: Value| CommandRequest {
        params: params.as_object().unwrap().clone(),
        ..Default::default()
    };
    let p = Uuid::from_u128(2);
    for name in ["task.show", "task.list"] {
        let c = r.get(name).unwrap();
        for route in [c.owner.as_ref().unwrap(), c.supervisor.as_ref().unwrap()] {
            let refused = c.forward(
                route,
                &req(json!({"project_id":p,"task_id":"t-1","bogus":1})),
                None,
            );
            assert_eq!(refused, Err(BuildError("unknown param")), "{name}");
        }
    }
    let list = r.get("task.list").unwrap();
    let f = list
        .forward(
            list.owner.as_ref().unwrap(),
            &req(json!({"project_id":p,"limit":5})),
            None,
        )
        .unwrap();
    assert_eq!(f.uri, format!("/api/human/projects/{p}/tasks?limit=5"));
}

fn action_registry() -> Registry {
    let kinds = [
        ("task.cancel", "task.cancel"),
        ("worker.revoke", "worker.control"),
        ("swarm.availability.set", "swarm.availability"),
        ("feedback.link", "result.comment"),
    ];
    let mut all = actions();
    for (kind, grant) in kinds {
        all.push(ActionSpec {
            kind,
            grant,
            target: TargetRule::Subject,
        });
    }
    Registry::new(&all).unwrap()
}

fn build_action(name: &str, params: Value, subject: Option<Value>) -> Result<Value, BuildError> {
    let r = action_registry();
    let op = Uuid::from_u128(7);
    let c = r.get(name).unwrap();
    let request = CommandRequest {
        operation_id: Some(op),
        subject,
        params: params.as_object().unwrap().clone(),
        ..Default::default()
    };
    c.forward(c.owner.as_ref().unwrap(), &request, None)
        .map(|f| f.body.unwrap())
}

#[test]
fn an_action_subject_follows_from_its_params() {
    let op = Uuid::from_u128(7);
    // An id the payload does not have is taken out of it.
    let b = build_action(
        "task.cancel",
        json!({"task_id":"t","project_id":"p","expected_version":"7"}),
        None,
    )
    .unwrap();
    assert_eq!(b["subject"], json!({"type":"task","id":"t","revision":"7"}));
    assert_eq!(b["expected_versions"], json!({"task":"7"}));
    assert_eq!(
        b["payload"],
        json!({"project_id":"p","expected_version":"7","operation_id":op})
    );
    // An id the payload has stays; no revision means no expected version.
    let b = build_action("worker.revoke", json!({"machine":"desktop"}), None).unwrap();
    assert_eq!(b["subject"], json!({"type":"machine","id":"desktop"}));
    assert_eq!(b["expected_versions"], json!({}));
    assert_eq!(b["payload"], json!({"machine":"desktop","operation_id":op}));
    let b = build_action("swarm.availability.set", json!({"available":true}), None).unwrap();
    assert_eq!(
        b["subject"],
        json!({"type":"swarm","id":"owner-availability"})
    );
    // A numeric revision is sent as its decimal string, under the kind's key.
    let b = build_action(
        "decision.resolve",
        json!({"decision_id":"d","expected_decision_revision":4}),
        None,
    )
    .unwrap();
    assert_eq!(
        b["subject"],
        json!({"type":"decision","id":"d","revision":"4"})
    );
    let b = build_action(
        "feedback.link",
        json!({"result_id":"r","expected_version":"2"}),
        None,
    )
    .unwrap();
    assert_eq!(b["expected_versions"], json!({"thread":"2"}));
}

#[test]
fn an_explicit_action_subject_overrides_the_derived_subject() {
    // An explicit subject is sent as given and the params are left alone.
    let given = json!({"type":"task","id":"other","revision":"9"});
    let b = build_action(
        "task.cancel",
        json!({"project_id":"p","expected_version":"9"}),
        Some(given.clone()),
    )
    .unwrap();
    assert_eq!(
        (b["subject"].clone(), b["expected_versions"].clone()),
        (given, json!({}))
    );
    // Without the id or the version nothing is built.
    assert!(build_action("task.cancel", json!({"expected_version":"7"}), None).is_err());
    assert!(build_action("task.cancel", json!({"task_id":"t"}), None).is_err());
}

#[test]
fn the_registry_publishes_action_subject_rules() {
    let r = action_registry();
    // The published entry says how.
    let doc = document(&r, &BTreeMap::new());
    let entry = doc["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "task.cancel")
        .unwrap();
    assert_eq!(
        entry["principals"]["owner"]["subject"],
        json!({"type":"task","id_param":"task_id","id_param_schema":{"type":"string","format":"uuid"},
            "revision_param":"expected_version","version_key":"task"})
    );
}

fn owner_mutation(name: &str, params: Value, reason: Option<&str>) -> Forward {
    let r = Registry::new(&actions()).unwrap();
    let op = Uuid::from_u128(9);
    let c = r.get(name).unwrap();
    assert!(
        c.kind == Kind::Mutation && c.owner_only() && c.agent.is_none(),
        "{name}"
    );
    let request = CommandRequest {
        operation_id: Some(op),
        reason: reason.map(str::to_owned),
        params: params.as_object().unwrap().clone(),
        ..Default::default()
    };
    c.forward(c.owner.as_ref().unwrap(), &request, None)
        .unwrap()
}

#[test]
fn route_backed_owner_mutations_fill_the_path_and_send_the_rest() {
    let op = Uuid::from_u128(9);
    // The request's operation ID and reason reach a body that takes them.
    let f = owner_mutation("attempt.cancel", json!({"attempt_id":"a-1"}), Some("stuck"));
    assert_eq!(
        (f.method, f.uri.as_str()),
        ("POST", "/api/execution/attempts/a-1/cancel")
    );
    assert_eq!(f.body.unwrap(), json!({"operation_id":op,"reason":"stuck"}));
    // Params win over the request's.
    let f = owner_mutation(
        "attempt.cancel",
        json!({"attempt_id":"a-1","operation_id":"mine","reason":"given"}),
        Some("other"),
    );
    assert_eq!(
        f.body.unwrap(),
        json!({"operation_id":"mine","reason":"given"})
    );
    // A body that takes neither gets neither: recover's must be `{}`.
    let f = owner_mutation("operation.recover", json!({"operation":"o-1"}), Some("why"));
    assert_eq!(f.uri, "/api/human/operations/o-1/recover");
    assert_eq!(f.body.unwrap(), json!({}));
    let f = owner_mutation(
        "session.manual.slots.set",
        json!({"node_id":"n","project_id":"p","expected_revision":2,"slots":1}),
        None,
    );
    assert_eq!(f.uri, "/api/manual-agents/workers/n/projects/p/slots");
    assert_eq!(
        f.body.unwrap(),
        json!({"operation_id":op,"expected_revision":2,"slots":1})
    );
}

#[test]
fn playbook_owner_mutations_forward_the_declared_method_and_path() {
    // A route that is a PUT or a DELETE is forwarded as one.
    let f = owner_mutation(
        "playbook.source.edit",
        json!({"source_id":"routing","markdown":"# m","expected_revision":3}),
        Some("why"),
    );
    assert_eq!(
        (f.method, f.uri.as_str()),
        ("PUT", "/api/playbooks/sources/routing/markdown")
    );
    assert_eq!(
        f.body.unwrap(),
        json!({"markdown":"# m","expected_revision":3})
    );
    let f = owner_mutation(
        "playbook.source.remove",
        json!({"source_id":"routing"}),
        None,
    );
    assert_eq!(
        (f.method, f.uri.as_str()),
        ("DELETE", "/api/playbooks/sources/routing")
    );
    assert_eq!(f.body.unwrap(), json!({}));
    // Two commands on one handler keep their own paths: it reads the path.
    let event = Uuid::from_u128(7);
    for (name, tail) in [
        ("playbook.event.retry", "retry"),
        ("playbook.event.hold", "hold"),
    ] {
        let f = owner_mutation(name, json!({"event_id":event}), None);
        assert_eq!(
            (f.method, f.uri),
            ("POST", format!("/api/playbooks/events/{event}/{tail}"))
        );
        assert_eq!(f.body.unwrap(), json!({}));
    }
}

#[test]
fn owner_mutation_schemas_require_their_path_parameters() {
    let r = Registry::new(&actions()).unwrap();
    // Each such schema is closed, names its path params and requires them.
    for c in r.commands() {
        let inline = |o: &Route| o.body == Body::Params && matches!(o.params, Params::Inline(_));
        let Some(route) = c.owner.filter(inline) else {
            continue;
        };
        let schema = params_schema(&route, &BTreeMap::new(), c.name);
        assert_eq!(schema["additionalProperties"], false, "{}", c.name);
        for (_, param) in route.segments() {
            assert!(
                schema["properties"].get(param).is_some(),
                "{}: {param}",
                c.name
            );
            assert!(
                schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(param)),
                "{}: {param}",
                c.name
            );
        }
    }
}

#[cfg(feature = "supervisor_full_access")]
#[test]
fn every_owner_command_has_a_supervisor_route() {
    let kept = [
        "whoami",
        "operation.show",
        "task.create",
        "task.graph.create",
        "task.edit",
        "task.reprioritize",
        "task.role.set",
        "task.route.set",
        "task.route.clear",
        "task.dependency.add",
        "task.dependency.remove",
        "project.list",
        "result.list",
        "result.show",
        "result.detail",
        "attempt.show",
    ];
    let r = Registry::new(&actions()).unwrap();
    for c in r.commands() {
        let Some(owner) = c.owner.filter(|o| o.body != Body::Local) else {
            continue;
        };
        let supervisor = c.supervisor.unwrap_or_else(|| panic!("{}", c.name));
        if kept.contains(&c.name) {
            assert!(!supervisor.path.starts_with(OWNER_PREFIX), "{}", c.name);
            continue;
        }
        assert_eq!(
            supervisor.path,
            format!("{OWNER_PREFIX}{}", owner.path),
            "{}",
            c.name
        );
        assert_eq!(
            Route {
                path: owner.path,
                ..supervisor
            },
            owner,
            "{}",
            c.name
        );
    }
}
