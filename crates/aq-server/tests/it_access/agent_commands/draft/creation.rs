use super::*;
impl Case {
    pub(super) async fn install_author(&self) {
        let nodes = &self.nodes;
        let (project_id, workspace_id, parent) =
            (id(&nodes.project), id(&nodes.workspace), id(&nodes.own));
        sqlx::query(
            "INSERT INTO task_git_authors
        (source,operation_id,project_id,workspace_id,task_id,requester_id,name,email)
        VALUES('human_graph',$1,$2,$3,$4,$5,'Original+Git','Original+Git@Example.Test')",
        )
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(workspace_id)
        .bind(parent)
        .bind(Uuid::new_v4())
        .execute(&self.e.pool)
        .await
        .unwrap();
    }
    fn batch(&self, count: usize, state: &str, edges: usize) -> Value {
        let own = &self.nodes.own;
        let tasks: Vec<_> = (0..count)
            .map(|n| {
                json!({"id":Uuid::new_v4(),"parent_id":own,
            "title":format!("Draft task {n}"),"task_state":state,"labels":["test"]})
            })
            .collect();
        let blocks: Vec<_> = (0..edges)
            .map(|n: usize| {
                json!({"id":Uuid::new_v4(),"source":tasks[n]["id"],
            "target":tasks[n+1]["id"]})
            })
            .collect();
        json!({"operation_id":Uuid::new_v4(),"params":{"expected_version":"1","tasks":tasks,"edges":blocks}})
    }
    pub(in super::super) async fn verify_batch_creation(&self) -> (Value, Value) {
        let project_id = id(&self.nodes.project);
        let workspace_id = id(&self.nodes.workspace);
        let denied = self
            .command("task.graph.create", self.batch(1, "active", 0))
            .await;
        assert_eq!(denied.0, StatusCode::FORBIDDEN, "{}", denied.1);
        let request = self.batch(12, "draft", 3);
        let (status, receipt) = self.command("task.graph.create", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
        for task in request["params"]["tasks"].as_array().unwrap() {
            let inherited = aq_human_work::task_git_author::lookup(
                &self.e.pool,
                project_id,
                workspace_id,
                id(&task["id"]),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(inherited.name, "Original+Git");
            assert_eq!(inherited.email, "Original+Git@Example.Test");
        }
        let (status, collision) = send(&self.e.app, "/v1/agent/graph/commands", Some(&self.e.token), Some(json!({
        "api_version":1,"operation_id":receipt["data"]["operation_id"],"binding":self.e.binding,
        "reason":"A different operation cannot shadow a draft receipt",
        "commands":[{"kind":"create_finding","id":Uuid::new_v4(),"title":"Collision","body":"","labels":[],"content_type":"note"}]
    }))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{collision}");

        (request, receipt)
    }
    pub(in super::super) async fn verify_budget_and_replay(&self, request: Value, receipt: Value) {
        let (_, replay) = self.command("task.graph.create", request).await;
        assert_eq!(replay["data"], receipt["data"]);
        let (_, filled) = self
            .command("task.graph.create", self.batch(18, "draft", 0))
            .await;
        assert_eq!(filled["data"]["disposition"], "confirmed", "{filled}");
        let (status, over) = self.command(
        "task.create",
        json!({"params":{"operation_id":Uuid::new_v4(),"parent_id":"self","title":"Task 31"}}),
    )
    .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{over}");
        assert_eq!(over["code"], "quota_exceeded", "{over}");
        {
            let writes = self.posted.lock().unwrap();
            assert_eq!(
                writes.len(),
                2,
                "Replay and refused batches never dispatch."
            );
            let ops = writes[0]["operations"].as_array().unwrap();
            assert_eq!(
                ops.iter()
                    .filter(|op| op["op"] == "create_node" && op["task_state"] == "draft")
                    .count(),
                12
            );
            assert_eq!(
                ops.iter().filter(|op| op["edge_type"] == "blocks").count(),
                3
            );
        }
    }
    pub(in super::super) async fn verify_unconfirmed_parent(&self) {
        let outside = self.nodes.outside;
        let attempt = self.attempt;
        let root = id(&self.nodes.own);
        sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms)
                 VALUES($1,$2,$3,$4,$4,1,$5,0)")
        .bind(outside).bind(attempt).bind(id(&self.nodes.project))
        .bind(root).bind(Uuid::new_v4()).execute(&self.e.pool).await.unwrap();
        // A conflicting or pending create can reserve an existing UUID, but
        // only a confirmed draft creation contributes subtree authority.
        assert_eq!(
            aq_agent_grants::store::depth(&self.e.pool, root, outside)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            aq_agent_grants::store::creator(&self.e.pool, outside)
                .await
                .unwrap(),
            None
        );
        self.set_grants("investigation").await;
        let (status, refused) = self.command(
        "task.create",
        json!({"params":{"operation_id":Uuid::new_v4(),"parent_id":"self","title":"No create"}}),
    )
    .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(refused["code"], "grant_missing");
    }
}
