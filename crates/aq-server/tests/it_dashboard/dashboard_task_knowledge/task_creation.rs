use super::*;

pub(super) fn graph_create(project: Uuid, first: Uuid, second: Uuid, edge: Uuid) -> Value {
    action(
        "task.graph.create",
        Subject::Project(project, "1"),
        json!({
            "operation_id":Uuid::new_v4(), "project_id":project, "expected_version":"1",
            "tasks":[
                {"id":first,"title":"First","body":"Implementation","priority":300,"role":"reviewer","intelligence_hint":"standard-high","task_type":"feature"},
                {"id":second,"title":"Second"}
            ],
            "edges":[{"id":edge,"source":first,"target":second}]
        }),
    )
}

pub(super) fn edge(id: Uuid, workspace: Uuid, source: Uuid, target: Uuid) -> Value {
    json!({"id":id,"workspace_id":workspace,"source":source,"target":target,"edge_type":"blocks","description":"","version":1,
        "source_revision":null,"target_revision":null,"deleted":false,"created_at":"2026-01-01T00:00:00Z"})
}

#[tokio::test]
async fn owner_creates_graph_with_metadata_and_retained_identity() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), a, b),
        ]),
    );
    let request = graph_create(x.project(), a, b, e);
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    assert_eq!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 3);
    let metadata = &posts[0]["operations"][0]["metadata"];
    assert_role_priority(metadata, "reviewer", 300);
    // A task that names no role stores none: the project's default applies.
    let unnamed = posts[0]["operations"][1]["metadata"].as_object().unwrap();
    assert!(!unnamed.contains_key("aq:role"), "{unnamed:?}");
    assert!(!posts[0].to_string().contains("profile_hint"));
    assert_eq!(metadata["aq:intelligence_hint"], "standard-high");
    assert_eq!(metadata["aq:task_type"], "feature");
    let (_, replay) = x.act(&request).await;
    assert_eq!(replay["data"], receipt["data"]);
    let mut changed = request.clone();
    changed["payload"]["tasks"][0]["priority"] = json!(1);
    assert_eq!(x.act(&changed).await.0, 409);
    assert_eq!(x.posts().len(), 1);
    let lookup = x
        .lookup(request["operation_id"].as_str().unwrap().parse().unwrap())
        .await;
    assert_eq!(lookup["data"], receipt["data"]);
    assert_task_authors(&x, [a, b]).await;
}

#[tokio::test]
async fn owner_single_create_accepts_enqueue_metadata() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let request = action(
        "task.create",
        Subject::Project(x.project(), "1"),
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Single","body":"","acceptance_criteria":"","labels":[],"task_state":"active",
            "priority":1,"role":"developer","intelligence_hint":"standard-high","task_type":"feature"
        }),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    let metadata = &x.posts()[0]["operations"][0]["metadata"];
    assert_role_priority(metadata, "developer", 1);
}

pub(super) fn single_create(x: &Fixture, id: Uuid, extra: Value) -> Value {
    let mut payload = json!({"operation_id":id,"expected_project_revision":"1","title":"Single","body":"",
        "acceptance_criteria":"","labels":[],"task_state":"active"});
    (payload.as_object_mut().unwrap()).extend(extra.as_object().unwrap().clone());
    action("task.create", Subject::Project(x.project(), "1"), payload)
}

#[tokio::test]
async fn a_graph_create_names_a_role_per_task_and_refuses_an_unknown_one() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    common::save_role(&pool, x.f.actor, "developer", "tester", true).await;
    let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    let graph = |roles: [Option<&str>; 3]| {
        let tasks: Vec<Value> = ids
            .iter()
            .zip(roles)
            .map(|(id, role)| match role {
                Some(role) => json!({"id":id,"title":"Task","role":role}),
                None => json!({"id":id,"title":"Task"}),
            })
            .collect();
        let payload = json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),
            "expected_version":"1","tasks":tasks});
        action(
            "task.graph.create",
            Subject::Project(x.project(), "1"),
            payload,
        )
    };
    let (status, body) = x
        .act(&graph([Some("reviewer"), Some("designer"), None]))
        .await;
    assert_unknown_role(
        status,
        &body,
        "Role `designer` does not exist. The roles are: developer, reviewer, tester.",
    );
    assert!(
        x.posts().is_empty(),
        "nothing of a refused graph is created"
    );
    let created = ids.iter().map(|id| node(*id, x.workspace(), "task", 1));
    x.reply(200, change_set(created.collect()));
    let (status, body) = x
        .act(&graph([Some("reviewer"), Some("tester"), None]))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operations = x.posts()[0]["operations"].clone();
    assert_eq!(operations[0]["metadata"]["aq:role"], "reviewer");
    assert_eq!(operations[1]["metadata"]["aq:role"], "tester");
    assert!(operations[2]["metadata"].get("aq:role").is_none());
}

#[tokio::test]
async fn graph_create_rejects_invalid_batches_before_any_effect() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let base = graph_create(x.project(), a, b, e);
    let mut cases = vec![];
    for priority in [0, 301] {
        let mut v = base.clone();
        v["payload"]["tasks"][0]["priority"] = json!(priority);
        cases.push(v);
    }
    let mut v = base.clone();
    v["payload"]["tasks"][1]["id"] = json!(a);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["edges"][0]["target"] = json!(a);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["tasks"] = json!([]);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["tasks"] = json!(
        (0..100)
            .map(|i| json!({"id":Uuid::new_v4(),"title":format!("{i}")}))
            .collect::<Vec<_>>()
    );
    cases.push(v);
    for request in cases {
        let (status, body) = x.act(&request).await;
        assert_eq!(status, 422, "{body}");
    }
    assert!(x.posts().is_empty());
    let mut stale = base;
    stale["payload"]["expected_version"] = json!("2");
    stale["subject"]["revision"] = json!("2");
    stale["expected_versions"]["project"] = json!("2");
    let (status, receipt) = x.act(&stale).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "conflict");
    assert!(x.posts().is_empty());
}

#[tokio::test]
async fn graph_create_accepts_existing_dependencies_and_refuses_other_projects() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let existing = Uuid::new_v4();
    let mut n = node(existing, x.workspace(), "task", 1);
    n["metadata"]["aq:project_id"] = json!(x.project());
    x.put(n.clone());
    let mut request = graph_create(x.project(), a, b, e);
    request["payload"]["edges"][0]["source"] = json!(existing);
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), existing, b),
        ]),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    assert_eq!(x.posts()[0]["operations"][2]["source"], json!(existing));
    n["metadata"]["aq:project_id"] = json!(Uuid::new_v4());
    x.put(n);
    request["operation_id"] = json!(Uuid::new_v4());
    request["payload"]["operation_id"] = request["operation_id"].clone();
    assert_eq!(x.act(&request).await.0, 403);
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn graph_create_retains_terminal_conflict_without_resend() {
    let x = setup().await;
    let request = graph_create(x.project(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(409, json!({"error":{"code":"conflict","message":"cycle"}}));
    let (_, receipt) = x.act(&request).await;
    assert_eq!(receipt["data"]["disposition"], "conflict");
    assert!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, replay) = x.act(&request).await;
    assert_eq!(replay["data"], receipt["data"]);
    let op = request["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(x.lookup(op).await["data"], receipt["data"]);
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn owner_single_refuses_cross_project_parent_before_dispatch() {
    let x = setup().await;
    let parent = Uuid::new_v4();
    let id = Uuid::new_v4();
    let mut n = node(parent, x.workspace(), "task", 1);
    n["metadata"]["aq:project_id"] = json!(Uuid::new_v4());
    x.put(n);
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let request = action(
        "task.create",
        Subject::Project(x.project(), "1"),
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Scoped child","body":"","acceptance_criteria":"","labels":[],"task_state":"active","parent_id":parent
        }),
    );
    let (status, body) = x.act(&request).await;
    assert_eq!(status, 403, "{body}");
    assert!(
        x.posts().is_empty(),
        "another project's parent is never dispatched"
    );
}

async fn assert_task_authors(x: &Fixture, tasks: [Uuid; 2]) {
    let pool = x.f.db.pool().await;
    for task in tasks {
        let author =
            aq_human_work::task_git_author::lookup(&pool, x.project(), x.workspace(), task)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(author.email, "owner@localhost");
        let response = aq_server::build_admin_router(x.f.state.clone())
            .oneshot(
                Request::get(format!("/api/human/projects/{}/tasks/{task}", x.project()))
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let reply: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            reply["data"]["requested_by"],
            json!({"name":"owner","email":"owner@localhost"})
        );
        assert_eq!(reply["data"]["created_by"], "owner@localhost");
    }
}

fn assert_role_priority(metadata: &Value, role: &str, priority: u64) {
    assert_eq!(metadata["aq:role"], role);
    assert_eq!(metadata["aq:priority"], priority);
}
