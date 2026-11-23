use super::*;

#[tokio::test]
async fn human_reads_attribute_service_writes_to_the_acting_owner() {
    // QA N-04: Graph records the one AQ mutation identity as creator and verifier.
    let x = setup().await;
    let op = Uuid::new_v4();
    x.reply(
        200,
        change_set(vec![node(op, x.workspace(), "knowledge", 1)]),
    );
    let (_, body) = x
        .act(&action(
"knowledge.author",
Subject::Project(x.project(), "1"),
json!({"operation_id":op,"project_id":x.project(),"expected_project_revision":"1","title":"Original title","body":"Original body"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut verified = x.graph.nodes.lock().unwrap()[&op].clone();
    verified["version"] = json!(2);
    verified["content_revision"] = json!(2);
    verified["verification"] = json!("verified");
    verified["last_verified_by"] = json!("dashboard-fixture-service");
    verified["last_verified_at"] = json!("2026-01-02T00:00:00Z");
    x.reply(200, change_set(vec![verified]));
    let mut verify = action(
        "knowledge.verify",
        Subject::Knowledge(op, "1"),
        json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","verification":"verified",
            "evidence":[{"source_id":"review-1","kind":"url","url":"https://example.test/review","observed_at":"2026-01-01T00:00:00Z","retained":false}]}),
    );
    verify["reason"] = json!("Reviewed");
    let (_, body) = x.act(&verify).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let other = Uuid::new_v4();
    x.put(node(other, x.workspace(), "knowledge", 1));
    let read = |id: Uuid| {
        let request = Request::get(format!(
            "/api/human/projects/{}/knowledge/{id}",
            x.project()
        ))
        .header("x-aq-dev-email", "owner@localhost")
        .body(Body::empty())
        .unwrap();
        let app = aq_server::build_admin_router(x.f.state.clone());
        async move {
            let res = app.oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()["data"].clone()
        }
    };
    let record = read(op).await;
    assert_eq!(record["created_by"], "owner@localhost", "{record}");
    assert_eq!(record["last_verified_by"], "owner@localhost", "{record}");
    let foreign = read(other).await;
    assert_eq!(
        foreign["created_by"], "fixture",
        "other actors keep their attribution"
    );
}

#[tokio::test]
async fn task_history_attributes_dashboard_writes_to_the_acting_owner() {
    // QA P1-07: history named the service identity for the owner's reprioritize.
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    let mut updated = node(task, x.workspace(), "task", 6);
    updated["metadata"]["aq:priority"] = json!(7);
    x.reply(200, change_set(vec![updated.clone()]));
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "task.reprioritize",
            Subject::Task(task, "5"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"5","priority":7}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let set = body["data"]["receipt_refs"][0]["id"].clone();
    let mut before = node(task, x.workspace(), "task", 5);
    before["content_revision"] = json!(5);
    let entry = |n: u64, actor: &str, change_set: Value, data: Value| {
        json!({"sequence":n,"entry_id":Uuid::new_v4(),"category":"revision","actor_id":actor,
            "operation":"updated","reason":format!("AQ dashboard action task.reprioritize {op}"),
            "change_set_id":change_set,"data":data,"created_at":chrono::Utc::now().to_rfc3339()})
    };
    *x.graph.history.lock().unwrap() = Some(json!({"items":[
        entry(1, "dashboard-fixture-service", json!(Uuid::new_v4()), before),
        entry(2, "dashboard-fixture-service", set, updated),
    ],"next_cursor":null}));
    let res = aq_server::build_admin_router(x.f.state.clone())
        .oneshot(
            Request::get(format!(
                "/api/human/projects/{}/tasks/{task}/history",
                x.project()
            ))
            .header("x-aq-dev-email", "owner@localhost")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    let items = &page["data"]["items"];
    assert_eq!(items[1]["actor_id"], "owner@localhost", "{page}");
    assert_eq!(
        items[0]["actor_id"], "dashboard-fixture-service",
        "a change set no owner operation confirmed keeps Graph's actor"
    );
    assert_eq!(
        items[1]["change"]["fields"],
        json!([{"field":"priority","from":null,"to":"7"}])
    );
}
