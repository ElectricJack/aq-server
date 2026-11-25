use super::*;
pub(super) async fn install(e: &mut Env, kind: &str) -> Uuid {
    let project = e.binding["project_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let workspace = e.binding["workspace_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let task = e.binding["task_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let namespace = Uuid::new_v4();
    Arc::make_mut(&mut e.state.config).profiled_execution = Some(
        serde_json::from_value(json!({
            "revision":1,"qualified_runtimes":[],"definition_bodies":[],
            "projects":[{"project_id":project,"enabled":true,"priority":0,
                "retention_handle":Uuid::new_v4(),
                "source":{"kind":"named_ref","reference":"refs/heads/main"},
                "agent_grants":{"by_work_kind":{
                    "investigation":{"max_created_tasks":0},
                    "fidelity_review":{"max_created_tasks":10,"edit_drafts":true}
                }}}]
        }))
        .unwrap(),
    );
    e.fake
        .data
        .lock()
        .await
        .nodes
        .get_mut(&task.to_string())
        .unwrap()["metadata"] = json!({"aq:project_id":project,"aq:playbook_namespace":namespace,
                "aq:logical_key":"test-policy","aq:work_kind":kind});
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest)
                     SELECT $1,1,'test-kind-policy',$2,$3,id,repeat('a',64),repeat('b',64) FROM users LIMIT 1")
            .bind(namespace).bind(workspace).bind(project).execute(&e.pool).await.unwrap();
    sqlx::query("INSERT INTO playbook_work_keys(namespace_id,generation,logical_key,task_id,request_digest,created_at_ms)
                     VALUES($1,1,'test-policy',$2,repeat('c',64),1)")
            .bind(namespace).bind(task).execute(&e.pool).await.unwrap();
    e.app = aq_server::build_worker_router(e.state.clone());
    task
}
pub(super) async fn denies_task_changes(e: &Env, kind: &str, task: Uuid) {
    let correction = command(
        e,
        json!([{"kind":"correct_record","id":task,
            "expected_version":"3","patch":{"title":"Attempted task bypass"}}]),
    );
    for request in [correction, proposal(e)] {
        let (status, denied) = submit(e, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{kind}: {denied}");
        assert_eq!(denied["code"], "scope_denied");
    }
    assert!(e.fake.data.lock().await.posts.is_empty());
}
pub(super) async fn permits_knowledge(e: &Env, kind: &str) {
    let (status, corrected) = submit(e, super::correction(e)).await;
    assert_eq!(status, StatusCode::OK, "{kind}: {corrected}");
    assert_eq!(corrected["state"], "confirmed");
    let (status, found) = submit(e, command(e, json!([{"kind":"create_finding",
            "id":Uuid::new_v4(),"title":"Retained finding","body":"","labels":[],"content_type":"note"}]))).await;
    assert_eq!(status, StatusCode::OK, "{kind}: {found}");
    assert_eq!(found["state"], "confirmed");
}
