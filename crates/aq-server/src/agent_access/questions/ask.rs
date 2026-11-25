use super::*;
use aq_wait_engine::question::QuestionWait;
use sqlx::postgres::PgRow;

/// Ask (or replay) one question for the authenticated attempt. The decision
/// and its wait commit together, or neither does.
pub async fn ask(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    submission: Submission<'_>,
) -> Result<Value> {
    if !submission.valid(MAX_QUESTION_BYTES) {
        return Err(QuestionError("invalid_request"));
    }
    let digest = wire::Digest::of(submission.text.as_bytes());
    let mut tx = state.store.pool.begin().await?;
    let caller = waits::load_caller(state, &mut tx, node, attempt).await?;
    if let Some(previous) = replay(&mut tx, attempt, &submission, &digest).await? {
        return Ok(previous);
    }
    let at = now(state);
    let wait = QuestionWait::new(Uuid::new_v4(), &caller.fence.binding, digest);
    let decision = NewQuestion {
        attempt,
        node,
        submission,
        caller: &caller,
        wait: &wait,
        at,
    };
    let row = decision.insert(&mut tx).await?;
    let request = wait.registration(&caller.fence, waits::default_policy().reference)?;
    let response = waits::register_locked(state, &mut tx, &caller, &request).await?;
    if !matches!(response.body, wire::ResponseBody::WaitRegistered(_)) {
        // A second active wait for the attempt rolls back this decision too.
        return Err(QuestionError("wait_conflict"));
    }
    tx.commit().await?;
    notify(
        state,
        aq_notifications::policy::Origin::new(submission.operation_id, caller.fence.scope.clone()),
        wait.decision_id,
        true,
    )
    .await;
    Ok(view(&row, false))
}
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    submission: &Submission<'_>,
    digest: &wire::Digest,
) -> Result<Option<Value>> {
    let row =
        sqlx::query("SELECT * FROM question_decisions WHERE attempt_id=$1 AND operation_id=$2")
            .bind(attempt)
            .bind(submission.operation_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("question_digest") != digest.as_str() {
        return Err(QuestionError("id_conflict"));
    }
    Ok(Some(view(&row, false)))
}
struct NewQuestion<'a> {
    node: Uuid,
    attempt: Uuid,
    submission: Submission<'a>,
    caller: &'a waits::Caller,
    wait: &'a QuestionWait,
    at: u64,
}
impl NewQuestion<'_> {
    async fn insert(&self, tx: &mut Transaction<'_, Postgres>) -> Result<PgRow> {
        let binding = &self.caller.fence.binding;
        let scope = &self.caller.fence.scope;
        let expires_at = self.at + aq_wait_engine::MAX_AGE_MS;
        Ok(sqlx::query("INSERT INTO question_decisions(decision_id,attempt_id,operation_id,node_id,project_id,workspace_id,task_id,session,attempt_generation,question,question_digest,subject_revision,decision_revision,state,expires_at_ms,wait_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,1,'pending',$13,$14,$15) RETURNING *")
        .bind(self.wait.decision_id)
        .bind(self.attempt)
        .bind(self.submission.operation_id)
        .bind(self.node)
        .bind(scope.project_id)
        .bind(scope.workspace_id)
        .bind(binding.task_id)
        .bind(serde_json::to_value(&binding.session)?)
        .bind(i64::try_from(binding.attempt_generation).map_err(|_| QuestionError("internal"))?)
        .bind(self.submission.text)
        .bind(self.wait.question_digest.as_str())
        .bind(&self.wait.subject_revision)
        .bind(i64::try_from(expires_at).map_err(|_| QuestionError("internal"))?)
        .bind(self.wait.wait_id)
        .bind(i64::try_from(self.at).map_err(|_| QuestionError("internal"))?)
        .fetch_one(&mut **tx)
        .await?)
    }
}
