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

#[test]
fn forward_builds_each_body_kind() {
    let r = Registry::new(&actions()).unwrap();
    let binding = json!({"attempt_id":"a"});
    let rid = Uuid::from_u128(1);
    let req = |params: Value| CommandRequest {
        params: params.as_object().unwrap().clone(),
        ..Default::default()
    };
    let show = r.get("task.show").unwrap();
    let p = Uuid::from_u128(2);
    // A declared query param follows the path params (queries.rs).
    let record = r.get("knowledge.show").unwrap();
    let f = forward(
        record,
        record.owner.as_ref().unwrap(),
        &req(json!({"project_id":p,"node_id":"t-1","revision":3})),
        None,
        rid,
    )
    .unwrap();
    assert_eq!(
        f.uri,
        format!("/api/human/projects/{p}/knowledge/t-1?revision=3")
    );
    assert!(f.body.is_none());
    let bad = forward(
        show,
        show.owner.as_ref().unwrap(),
        &req(json!({"project_id":"../x","task_id":"t"})),
        None,
        rid,
    );
    assert!(bad.is_err(), "path params are plain identifiers");
    let ask = r.get("question.ask").unwrap();
    let f = forward(
        ask,
        ask.agent.as_ref().unwrap(),
        &req(json!({"operation_id":"o","question":"q?"})),
        None,
        rid,
    )
    .unwrap();
    assert_eq!(
        f.body.unwrap(),
        json!({"api_version":1,"operation_id":"o","question":"q?"})
    );
    let search = r.get("knowledge.search").unwrap();
    let f = forward(
        search,
        search.agent.as_ref().unwrap(),
        &req(json!({"text":"release"})),
        Some(&binding),
        rid,
    )
    .unwrap();
    assert_eq!(
        f.body.unwrap(),
        json!({"api_version":1,"request_id":rid,"binding":binding,"query":{"kind":"search","text":"release","kind_filter":null,"limit":20,"cursor":null}})
    );
    let create = r.get("task.create").unwrap();
    let op = Uuid::from_u128(3);
    let mut request = req(json!({"title":"x"}));
    request.operation_id = Some(op);
    request.subject = Some(json!({"type":"project","id":"p","revision":"4"}));
    let f = forward(create, create.owner.as_ref().unwrap(), &request, None, rid).unwrap();
    assert_eq!((f.method, f.uri.as_str()), ("POST", "/api/v1/actions"));
    let body = f.body.unwrap();
    assert_eq!(
        (body["kind"].clone(), body["operation_id"].clone()),
        (json!("task.create"), json!(op))
    );
    assert_eq!(body["payload"], json!({"title":"x"}));
    request.subject = None;
    assert!(forward(create, create.owner.as_ref().unwrap(), &request, None, rid).is_err());
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

#[test]
fn agent_task_reads_are_scoped_graph_reads_of_the_named_task() {
    use aq_agent_grants::{Decision, Grant, GrantSet, Holder, Relation, Scope, Target};
    let r = Registry::new(&actions()).unwrap();
    let (project, own, other) = (Uuid::from_u128(7), Uuid::from_u128(8), Uuid::from_u128(9));
    let holder = Holder { project };
    let binding = json!({"attempt_id":"a"});
    let rid = Uuid::from_u128(1);
    let grants = GrantSet::new(vec![
        Grant::new("task.read", Scope::TaskSubtree),
        Grant::new("comment.write", Scope::TaskSubtree),
    ])
    .unwrap();
    let run = |name: &str, params: Value, rel: Relation| {
        let spec = r.get(name).unwrap();
        let mut params = params.as_object().unwrap().clone();
        let (d, t) = authorize(spec, &mut params, &grants, holder, own, rel);
        let request = CommandRequest {
            params,
            ..Default::default()
        };
        let f = forward(
            spec,
            spec.agent.as_ref().unwrap(),
            &request,
            Some(&binding),
            rid,
        );
        (d, t, f.map(|f| f.body.unwrap()["query"].clone()))
    };
    // Omitted: the attempt's own task, sent as the graph node.
    let (d, t, q) = run("task.show", json!({}), Relation::Other);
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
    let (d, _, _) = run("task.show", json!({"task_id":other}), Relation::Other);
    assert!(matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::ScopeDenied));
    let (d, _, q) = run(
        "task.children",
        json!({"task_id":other,"project_id":project}),
        Relation::Descendant(1),
    );
    assert!(matches!(d, Decision::Allow(_)));
    assert_eq!(
        q.unwrap(),
        json!({"kind":"relationships","node_id":other,"direction":"outgoing","edge_types":["parent-child"],"limit":64,"cursor":null})
    );
    let (_, _, q) = run("task.history", json!({"limit":5}), Relation::Other);
    assert_eq!(
        q.unwrap(),
        json!({"kind":"history","node_id":own,"limit":5,"cursor":null})
    );
    // Another project is outside every agent scope but an owner override.
    let (d, t, _) = run(
        "task.show",
        json!({"project_id":Uuid::from_u128(5)}),
        Relation::Own,
    );
    assert!(matches!(d, Decision::Deny(_)));
    assert_eq!(t, Target::Project(Uuid::from_u128(5)));
    // A comment the caller did not write resolves outside every scope.
    let c = json!({"comment_id":Uuid::from_u128(4),"expected_revision":"1","operation_id":rid});
    let (d, _, _) = run("task.comment.delete", c.clone(), Relation::Other);
    assert!(matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::ScopeDenied));
    let (d, _, _) = run("task.comment.delete", c, Relation::Own);
    assert!(matches!(d, Decision::Allow(_)));
    let (d, _, _) = run(
        "task.comment.edit",
        json!({"comment_id":"x"}),
        Relation::Own,
    );
    assert!(
        matches!(d, Decision::Deny(r) if r.code == aq_agent_grants::RefusalCode::InvalidRequest)
    );
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
        if owner.body == Body::Query
            && supervisor.body == Body::Query
            && supervisor.path.ends_with(owner.path)
        {
            assert_eq!(owner.query(), supervisor.query(), "{}", c.name);
        }
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
            let refused = forward(
                c,
                route,
                &req(json!({"project_id":p,"task_id":"t-1","bogus":1})),
                None,
                Uuid::nil(),
            );
            assert_eq!(refused, Err(BuildError("unknown param")), "{name}");
        }
    }
    let list = r.get("task.list").unwrap();
    let f = forward(
        list,
        list.owner.as_ref().unwrap(),
        &req(json!({"project_id":p,"limit":5})),
        None,
        Uuid::nil(),
    )
    .unwrap();
    assert_eq!(f.uri, format!("/api/human/projects/{p}/tasks?limit=5"));
}
