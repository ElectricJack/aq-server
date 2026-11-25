mod settings;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub(crate) use settings::routes as settings_routes;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const EVIDENCE: &str = include_str!("outcomes/evidence.sql");

pub(super) async fn summary(
    pool: &PgPool,
    project: Option<Uuid>,
    cutoff: i64,
) -> anyhow::Result<Value> {
    evidence(pool, project, cutoff, None).await
}

/// Call after the usual task visibility check. The project is mandatory
/// for a scoped viewer. An authorized fleet reader may omit it.
pub(super) async fn first_pass(
    pool: &PgPool,
    project: Option<Uuid>,
    task: Uuid,
    cutoff: i64,
) -> anyhow::Result<Value> {
    let evidence = evidence(pool, project, cutoff, Some(task)).await?;
    Ok(evidence["tasks"][0].clone())
}

async fn evidence(
    pool: &PgPool,
    project: Option<Uuid>,
    cutoff: i64,
    task: Option<Uuid>,
) -> anyhow::Result<Value> {
    Ok(sqlx::query_scalar(EVIDENCE)
        .bind(project)
        .bind(cutoff.saturating_sub(WEEK_MS))
        .bind(cutoff)
        .bind(task)
        .fetch_one(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDb;
    use serde_json::json;

    #[tokio::test]
    async fn merge_evidence_is_scoped_deduplicated_and_uses_the_first_ready_definition() {
        let database = TestDb::create().await;
        let _store = database.store().await;
        let pool = database.pool().await;
        let project = Uuid::new_v4();
        let other = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let tracked = Uuid::new_v4();
        let legacy = Uuid::new_v4();
        let cutoff = WEEK_MS;
        let ready = cutoff - 360_000;

        sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'metrics@example.org','owner',now())")
            .bind(owner).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation)
             VALUES($1,1,$2,1,'{}')",
        )
        .bind(project)
        .bind(json!({"repository_id": repository}))
        .execute(&pool)
        .await
        .unwrap();
        let without_definition = summary(&pool, Some(project), cutoff).await.unwrap();
        assert_eq!(
            without_definition["delivery"]["delivered_tasks"],
            Value::Null
        );

        sqlx::query(
            "INSERT INTO performance_delivery_definitions(project_id,work_class,revision,effective_at_ms,definition,operation_id,actor_id)
             VALUES($1,'',1,$2,$3,$4,$5),($1,'',2,$6,$7,$8,$5)",
        )
        .bind(project)
        .bind(ready - 100)
        .bind(json!({"kind":"target_branch_merge","repository_id":repository,"target_ref":"refs/heads/main"}))
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(ready + 100)
        .bind(json!({"kind":"confirmed_completion"}))
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO performance_task_first_ready(project_id,task_id,ready_at_ms) VALUES($1,$2,$3)",
        )
        .bind(project)
        .bind(tracked)
        .bind(ready)
        .execute(&pool)
        .await
        .unwrap();

        // First verified receipt, replay with a different operation, an
        // unrelated target, a historical task with no ready clock, and a
        // separate project containing the same task ID.
        for (scope, task, pr, branch, time) in [
            (project, tracked, 1_i64, "main", ready + 60_000),
            (project, tracked, 1, "main", ready + 90_000),
            (project, tracked, 2, "release", ready + 120_000),
            (project, legacy, 3, "main", ready + 180_000),
            (other, tracked, 4, "main", ready + 240_000),
        ] {
            sqlx::query(
                "INSERT INTO delivery_merge_evidence(operation_id,project_id,task_id,pull_request,target,
                 target_ref,expected_head_sha,merge_sha,observed_at_ms)
                 VALUES($1,$2,$3,$4,$5,$6,repeat('a',40),repeat('b',40),$7)",
            )
            .bind(Uuid::new_v4())
            .bind(scope)
            .bind(task)
            .bind(pr)
            .bind(json!({"git":{"repository_id":repository},"github":{"owner":"org","name":"repo"}}))
            .bind(format!("refs/heads/{branch}"))
            .bind(time)
            .execute(&pool)
            .await
            .unwrap();
        }
        let outcomes = summary(&pool, Some(project), cutoff).await.unwrap();
        assert_eq!(outcomes["confirmed_completions"], 0);
        assert_eq!(outcomes["verified_merges"], 3);
        assert_eq!(
            outcomes["merge_timing"],
            json!({"total":3,"covered":2,"median_ms":90_000,"p90_ms":114_000})
        );
        assert_eq!(outcomes["delivery"]["state"], "partial");
        assert_eq!(outcomes["delivery"]["unknown_definitions"], 1);
        assert_eq!(outcomes["delivery"]["delivered_tasks"], 1);
        assert_eq!(outcomes["delivery"]["timing"]["median_ms"], 60_000);
        assert_eq!(outcomes["first_pass_acceptance"]["value"], Value::Null);
        let task = first_pass(&pool, Some(project), tracked, cutoff)
            .await
            .unwrap();
        assert_eq!(task["definition_revision"], 1);
        assert_eq!(task["delivery_at_ms"], ready + 60_000);
        assert_eq!(task["disposition"], "undetermined");
    }
}
