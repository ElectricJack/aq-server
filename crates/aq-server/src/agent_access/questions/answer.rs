use super::*;
use sqlx::postgres::PgRow;
/// Record the owner's answer at `expected_revision`. Replay of the same owner
/// operation with the same text returns the original receipt.
pub async fn answer(
    state: &AppState,
    owner: Uuid,
    decision: DecisionRevision,
    submission: Submission<'_>,
) -> Result<Value> {
    if !submission.valid(MAX_ANSWER_BYTES) {
        return Err(QuestionError("invalid_request"));
    }
    let mut tx = state.store.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM question_decisions WHERE decision_id=$1 FOR UPDATE")
        .bind(decision.id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(QuestionError("not_found"))?;
    let answer = Answer {
        owner,
        decision,
        submission,
    };
    if row.get::<String, _>("state") == "answered" {
        return answer.replay(&row);
    }
    let revision = answer.current_revision(&row)?;
    require_owner(&mut tx, owner).await?;
    let next = revision + 1;
    let at = now(state);
    let receipt_digest = submission.receipt_digest(owner, decision.id, next)?;
    let scope = wire::Scope {
        project_id: row.get("project_id"),
        workspace_id: row.get("workspace_id"),
    };
    let recorded_at = AnswerTime { next, at };
    let delivery = answer.delivery(&mut tx, &row, recorded_at).await?;
    let recorded = RecordedAnswer {
        time: recorded_at,
        digest: receipt_digest,
        delivery,
    };
    let row = answer.write(&mut tx, recorded).await?;
    tx.commit().await?;
    notify(
        state,
        aq_notifications::policy::Origin::new(submission.operation_id, scope),
        decision.id,
        false,
    )
    .await;
    Ok(receipt(&row))
}

struct Answer<'a> {
    owner: Uuid,
    decision: DecisionRevision,
    submission: Submission<'a>,
}
#[derive(Clone, Copy)]
struct AnswerTime {
    next: i64,
    at: u64,
}
struct RecordedAnswer {
    time: AnswerTime,
    digest: String,
    delivery: Value,
}
impl Answer<'_> {
    fn replay(&self, row: &PgRow) -> Result<Value> {
        let owner = row.get::<Option<Uuid>, _>("answered_by") == Some(self.owner);
        let operation =
            row.get::<Option<Uuid>, _>("answer_operation_id") == Some(self.submission.operation_id);
        let text = row.get::<Option<String>, _>("answer").as_deref() == Some(self.submission.text);
        let same_operation = owner && operation;
        if same_operation && text {
            Ok(receipt(row))
        } else {
            Err(QuestionError("id_conflict"))
        }
    }
    fn current_revision(&self, row: &PgRow) -> Result<i64> {
        let revision = row.get::<i64, _>("decision_revision");
        if u64::try_from(revision).ok() != Some(self.decision.revision) {
            return Err(QuestionError("stale_revision"));
        }
        Ok(revision)
    }
    async fn delivery(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        row: &PgRow,
        AnswerTime { next, at }: AnswerTime,
    ) -> Result<Value> {
        if !ended(tx, row.get("attempt_id")).await? {
            return Ok(json!({"kind":"wait"}));
        }
        let Submission {
            operation_id,
            text: answer,
        } = self.submission;
        let decision_id = self.decision.id;
        let message_id = Uuid::new_v5(&operation_id, b"question-answer-message");
        let recipient = Recipient::Task {
            workspace_id: row.get("workspace_id"),
            project_id: row.get("project_id"),
            task_id: row.get("task_id"),
        };
        let message = Send {
            operation_id,
            message_id,
            thread_id: decision_id,
            recipient,
            body: format!(
                "AQ answer to your question (decision {decision_id}, revision {next}):\n\n{answer}"
            ),
        };
        question_send_in_tx(tx, message, at)
            .await
            .map_err(mail_error)?;
        Ok(json!({"kind":"task_mail","message_id":message_id}))
    }
    async fn write(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        recorded: RecordedAnswer,
    ) -> Result<PgRow> {
        sqlx::query("UPDATE question_decisions SET state='answered',decision_revision=$2,answer=$3,answered_by=$4,answer_operation_id=$5,receipt_digest=$6,answered_at_ms=$7,delivery=$8 WHERE decision_id=$1 RETURNING *")
            .bind(self.decision.id)
            .bind(recorded.time.next)
            .bind(self.submission.text)
            .bind(self.owner)
            .bind(self.submission.operation_id)
            .bind(recorded.digest)
            .bind(i64::try_from(recorded.time.at).map_err(|_|QuestionError("internal"))?)
            .bind(recorded.delivery)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error|match error {
                sqlx::Error::Database(e) if e.is_unique_violation()=>QuestionError("id_conflict"),
                _=>QuestionError("internal"),
            })
    }
}
async fn require_owner(tx: &mut Transaction<'_, Postgres>, owner: Uuid) -> Result<()> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?;
    if role.as_deref() == Some("owner") {
        Ok(())
    } else {
        Err(QuestionError("scope_denied"))
    }
}
fn mail_error(error: OwnerSendError) -> QuestionError {
    match error {
        OwnerSendError::Invalid => QuestionError("invalid_request"),
        OwnerSendError::NotFound => QuestionError("not_found"),
        OwnerSendError::Conflict => QuestionError("id_conflict"),
        OwnerSendError::Unknown => QuestionError("internal"),
    }
}
fn receipt(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "decision_id": row.get::<Uuid, _>("decision_id"),
        "decision_revision": row.get::<i64, _>("decision_revision").to_string(),
        "receipt_digest": row.get::<Option<String>, _>("receipt_digest"),
        "delivery": row.get::<Option<Value>, _>("delivery"),
    })
}
/// The asking attempt can no longer receive a wait wake: it released the task,
/// was cancelled, or its launch binding was revoked. A merely stopped attempt
/// with a live wait is a hibernated continuation and still wakes by the wait.
async fn ended(tx: &mut Transaction<'_, Postgres>, attempt: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT NOT e.holds_task OR e.state->>'cancellation_id' IS NOT NULL OR COALESCE(b.revoked,FALSE) FROM execution_attempts e LEFT JOIN agent_launch_bindings b USING(attempt_id) WHERE e.attempt_id=$1")
        .bind(attempt)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(true))
}
