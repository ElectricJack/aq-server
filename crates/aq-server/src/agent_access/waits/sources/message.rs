use super::*;
use sqlx::postgres::PgRow;

pub(super) struct Thread<'a> {
    pub id: Uuid,
    pub after: u64,
    pub sender: &'a wire::SenderConstraint,
}
impl Thread<'_> {
    pub(super) async fn read(
        self,
        tx: &mut Transaction<'_, Postgres>,
        request: &SourceRequest,
    ) -> Result<Option<Observed>> {
        let Self {
            id: thread_id,
            after: exclusive_cursor,
            sender,
        } = self;
        // Task-addressed mailbox only: held for any successor attempt, never consumed.
        let key = Recipient::Task {
            workspace_id: request.scope.workspace_id,
            project_id: request.binding.project_id,
            task_id: request.binding.task_id,
        }
        .key()
        .map_err(|_| super::super::WaitError("internal"))?;
        let after =
            i64::try_from(exclusive_cursor).map_err(|_| super::super::WaitError("internal"))?;
        let rows = sqlx::query("SELECT message_id,cursor,sender,payload_digest FROM mailbox_messages WHERE mailbox_key=$1 AND thread_id=$2 AND cursor>$3 ORDER BY cursor LIMIT 256")
        .bind(&key)
        .bind(thread_id)
        .bind(after)
        .fetch_all(&mut **tx)
        .await?;
        let head: Option<i64> =
            sqlx::query_scalar("SELECT next_cursor FROM mailbox_heads WHERE mailbox_key=$1")
                .bind(&key)
                .fetch_optional(&mut **tx)
                .await?;
        let found = rows
            .iter()
            .find(|row| sender_matches(sender, row.get("sender")));
        let revision = head.unwrap_or(0).to_string();
        let snapshot = snapshot(
            format!("mailbox-thread-{thread_id}"),
            &json!({"mailbox":key,"thread_id":thread_id,"head":revision}),
        )?;
        Ok(Some(match found {
            Some(row) => Observed {
                state: wire::MemberState::Satisfied,
                result: Some(message_receipt(row)?),
                revision,
                snapshot,
            },
            None => Observed {
                state: wire::MemberState::Pending,
                result: None,
                revision,
                snapshot,
            },
        }))
    }
}

fn sender_matches(sender: &wire::SenderConstraint, from: Value) -> bool {
    match sender {
        wire::SenderConstraint::AnyAuthorized => true,
        wire::SenderConstraint::Task { task_id } => from["binding"]["task_id"] == json!(task_id),
        wire::SenderConstraint::Session { binding } => {
            from["binding"]["attempt_id"] == json!(binding.attempt_id)
        }
    }
}

fn message_receipt(row: &PgRow) -> Result<wire::ArtifactRef> {
    let message_id: Uuid = row.get("message_id");
    let payload: String = row.get("payload_digest");
    Ok(artifact(
        format!("mailbox-message-{message_id}"),
        value_digest(
            &json!({"message_id":message_id,"payload_digest":payload,"cursor":row.get::<i64, _>("cursor")}),
        )?,
    ))
}
