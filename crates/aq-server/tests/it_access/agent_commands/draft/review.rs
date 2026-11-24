use super::*;
impl Case {
    pub(in super::super) async fn verify_review_motivation(&self) {
        let Nodes {
            draft,
            namespace,
            planner,
            ..
        } = self.nodes;
        let Nodes {
            ref project,
            ref workspace,
            ref own,
            ..
        } = self.nodes;
        self.set_grants("fidelity_review").await;
        let (status, fake) = self.command("task.edit", change(draft)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Forged metadata: {fake}");
        let project_id: Uuid = project.as_str().unwrap().parse().unwrap();
        let workspace_id: Uuid = workspace.as_str().unwrap().parse().unwrap();
        sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest)
                 SELECT $1,1,'test-draft-namespace',$2,$3,id,repeat('a',64),repeat('b',64) FROM users LIMIT 1")
        .bind(namespace).bind(workspace_id).bind(project_id).execute(&self.e.pool).await.unwrap();
        sqlx::query("INSERT INTO playbook_work_keys(namespace_id,generation,logical_key,task_id,request_digest,created_at_ms)
                 VALUES ($1,1,'test-review',$2,repeat('c',64),1)")
        .bind(namespace).bind(own.as_str().unwrap().parse::<Uuid>().unwrap()).execute(&self.e.pool).await.unwrap();
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,created_at_ms)
                 VALUES($1,$2,1,'test-review-intent','work.ensure',$3,1,$4,'running',1)")
        .bind(Uuid::new_v4()).bind(namespace).bind(own.as_str().unwrap())
        .bind(json!({"kind":"fidelity_review","motivation":{"id":planner,"revision":1}}))
        .execute(&self.e.pool).await.unwrap();
        let (status, edit) = self.command("task.edit", change(draft)).await;
        assert_eq!(status, StatusCode::OK, "{edit}");
        assert_eq!(edit["data"]["disposition"], "confirmed", "{edit}");
    }
    pub(in super::super) async fn verify_refused_endpoints(&self) {
        let Nodes {
            draft,
            outside,
            active,
            foreign,
            ref unrelated_edge,
            ref foreign_blocker,
            ..
        } = self.nodes;
        for (name, request) in [
            ("task.edit", change(outside)),
            ("task.edit", change(active)),
            ("task.edit", change(foreign)),
            (
                "task.dependency.add",
                json!({"operation_id":Uuid::new_v4(),"params":{"task_id":draft,"expected_version":"1","blocker_id":outside}}),
            ),
            (
                "task.dependency.remove",
                json!({"operation_id":Uuid::new_v4(),"params":{"task_id":draft,"expected_version":"1","edge_id":foreign_blocker["id"],"edge_version":"1"}}),
            ),
            (
                "task.dependency.remove",
                json!({"operation_id":Uuid::new_v4(),"params":{"task_id":draft,"expected_version":"1","edge_id":unrelated_edge["id"],"edge_version":"1"}}),
            ),
        ] {
            let (status, body) = self.command(name, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {body}");
            assert_eq!(body["code"], "scope_denied", "{body}");
        }
    }
    pub(in super::super) async fn verify_dependency_effects(&self) {
        let Nodes {
            draft,
            sibling,
            planner,
            ref unrelated_edge,
            ..
        } = self.nodes;
        let (status, dependency) = self
            .command(
                "task.dependency.add",
                json!({"operation_id":Uuid::new_v4(),
        "params":{"task_id":draft,"expected_version":"1","blocker_id":sibling}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{dependency}");
        assert_eq!(dependency["data"]["disposition"], "confirmed");
        let (status, lookup) = self
            .command(
                "operation.show",
                json!({"params":{
        "operation_id":dependency["data"]["operation_id"]}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{lookup}");
        assert_eq!(lookup["data"], dependency["data"]);
        {
            let writes = self.posted.lock().unwrap();
            let patch = writes.last().unwrap()["operations"].as_array().unwrap();
            for id in [draft, sibling, planner] {
                assert!(
                    patch.iter().any(|op| op["op"] == "update_node"
                        && op["id"] == json!(id)
                        && op["patch"] == json!({})),
                    "Missing version guard for {id}: {patch:?}"
                );
            }
        }
        let (status, removed) = self.command("task.dependency.remove", json!({"operation_id":Uuid::new_v4(),
        "params":{"task_id":sibling,"expected_version":"1","edge_id":unrelated_edge["id"],"edge_version":"1"}})).await;
        assert_eq!(status, StatusCode::OK, "{removed}");
        assert_eq!(removed["data"]["disposition"], "confirmed");
        let (status, cancelled) = self
            .command(
                "task.cancel",
                json!({"operation_id":Uuid::new_v4(),
        "params":{"task_id":sibling,"expected_version":"1"}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        assert_eq!(cancelled["data"]["disposition"], "confirmed");
    }
}
