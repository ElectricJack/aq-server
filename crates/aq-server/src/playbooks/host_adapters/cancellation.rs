use super::*;

impl Service {
    pub(super) async fn operation_cancel(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let handle = text(&intent.input, "handle")?.to_string();
        let (stage, identity) = self
            .prepare_effect(owner, command, Effect::Cancel(&handle))
            .await?;
        let plan = match stage {
            PreparedEffect::Complete(result) => return Ok(result),
            PreparedEffect::Pending(plan) => plan,
        };
        if !plan["check"].is_null() {
            // The executor stop alone completes the check; an ambiguous
            // executor cancel leaves the plan pending for a resumed dispatch.
            let check: Uuid = serde_json::from_value(plan["check"].clone())?;
            let outcome = self.cancel_check(check).await?;
            return self
                .finish(
                    intent,
                    &identity,
                    HostResult {
                        outcome: outcome.into(),
                        output: json!({"reference": handle, "subject": plan["subject"]}),
                    },
                )
                .await;
        }
        let attempt: Uuid = serde_json::from_value(plan["attempt"].clone())?;
        let operation: Uuid = serde_json::from_value(plan["operation"].clone())?;
        use crate::profiled_execution::cancellation::{Disposition, cancel};
        let reason = format!("playbook operation.cancel {}", self.namespace.namespace_id);
        // B3 owns cancellation; completed effects are never undone here.
        let (outcome, authority) = match cancel(&ports.state, attempt, operation, &reason).await {
            Ok(receipt) => (
                match receipt.disposition {
                    Disposition::Requested => "accepted",
                    Disposition::NeverGranted => "cancelled",
                    Disposition::AlreadyAccepted | Disposition::AlreadyPublishing => "too_late",
                },
                i64::try_from(receipt.authority_revision).map_err(|_| Error::Unknown)?,
            ),
            Err(crate::error::AppError::NotFound(_) | crate::error::AppError::BadRequest(_)) => {
                ("refused", 0)
            }
            Err(crate::error::AppError::Conflict(_)) => ("indeterminate", 0),
            Err(_) => return Err(Error::Unknown),
        };
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: outcome.into(),
                output: json!({"reference": handle, "subject": subject(attempt, authority)}),
            },
        )
        .await
    }

    pub(super) async fn cancel_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        handle: &str,
    ) -> Result<PreparedEffect> {
        if self.profile.is_host2()
            && let Ok(check) = Uuid::parse_str(handle)
        {
            if let Some(plan) = self.check_plan(tx, check).await? {
                return Ok(PreparedEffect::Pending(plan));
            }
        }
        let Some(attempt) = self.cancel_target(tx, handle).await? else {
            return Ok(PreparedEffect::Complete(host_result(
                "refused",
                json!({"reference":handle,"subject":subject(handle,0)}),
            )));
        };
        Ok(PreparedEffect::Pending(
            json!({"attempt":attempt,"operation":self.operation(intent)?}),
        ))
    }
    async fn cancel_target(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        handle: &str,
    ) -> Result<Option<Uuid>> {
        if let Some(attempt) = handle
            .strip_prefix(ATTEMPT_REF)
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            let db = &mut **tx;
            let project: Option<Uuid> = sqlx::query_scalar("SELECT o.project_id FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) JOIN admission_source_observations o ON o.observation_id=l.source_observation_id WHERE a.attempt_id=$1 AND a.kind='profiled_v1'")
                .bind(attempt)
                .fetch_optional(&mut *db)
                .await?;
            return Ok((project == Some(self.namespace.project_id)).then_some(attempt));
        }
        let Some(task) = handle
            .strip_prefix(WORK_REF)
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return Ok(None);
        };
        if !self.namespace_work(tx, task).await? {
            return Ok(None);
        }
        let db = &mut **tx;
        Ok(sqlx::query_scalar::<_, Uuid>("SELECT attempt_id FROM execution_attempts WHERE task_id=$1 AND holds_task AND kind='profiled_v1'")
                .bind(task)
                .fetch_optional(&mut *db)
                .await?)
    }
}
