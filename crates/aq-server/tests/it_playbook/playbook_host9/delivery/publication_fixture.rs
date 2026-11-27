//! The forge tests use a synthetic result port. Mirror its exact publication
//! identity in storage so the production dispatch fence is exercised as well.
use super::*;

impl D {
    pub(super) async fn seed_publication_result(&self, request: &Value) {
        let uuid = |value: &Value| Uuid::parse_str(value.as_str().unwrap()).unwrap();
        let attempt = uuid(&request["result_ready"]["attempt_id"]);
        let report = uuid(&request["result_ready"]["result_id"]);
        let task = uuid(&request["task"]);
        let target = &request["publication_target"]["git"];
        let repository = uuid(&target["repository_id"]);
        let branch = aq_delivery::commands::task_branch(task);
        let spec = json!({"lifecycle":{"project_id":request["project"]},
            "source":{"repository_id":repository,"repository_binding_generation":target["binding_generation"]}});
        let pool = self.pool().await;
        let mut tx = pool.begin().await.unwrap();
        // Only fixture construction bypasses owner guards. Dispatch runs with
        // normal constraints after commit; no result is submitted or accepted.
        sqlx::query("SET LOCAL session_replication_role=replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',jsonb_build_object('schema_version','1','status','finished')) ON CONFLICT DO NOTHING")
            .bind(attempt).bind(task).bind(self.f.node.node_id).bind(Uuid::new_v4()).bind(Uuid::new_v4())
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO profiled_launches SELECT (jsonb_populate_record(NULL::profiled_launches,to_jsonb(l)||jsonb_build_object('attempt_id',$2::uuid,'execution_spec',$3::jsonb))).* FROM profiled_launches l WHERE attempt_id=$1 ON CONFLICT DO NOTHING")
            .bind(self.f.profiled.offer.binding.attempt_id).bind(attempt).bind(spec)
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,report_bytes,report_digest,receipt_bytes,outcome_artifact_id) VALUES($1,$2,$3,$4,$5,encode(sha256($5),'hex'),$5,$4) ON CONFLICT DO NOTHING")
            .bind(report).bind(attempt).bind(self.f.node.node_id).bind(Uuid::new_v4()).bind(br#"{"outcome":"success"}"#.as_slice())
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(attempt).bind(repository).bind("b".repeat(40)).bind(&branch)
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO delivery_pushed_results(report_id,attempt_id,task_ref,head) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(report).bind(attempt).bind(branch).bind(request["result_head"].as_str().unwrap())
            .execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
}
