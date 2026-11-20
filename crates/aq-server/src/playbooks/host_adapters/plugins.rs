//! The retained host effect is the commit point for the plugin result and events.
use super::{PLUGIN_COMPLETED, Stage, *};
use aq_playbook_host::plugins::{Call, Invocation, Response, command_plugin};
use chrono::{DateTime, Utc};

fn refused(reason: &str) -> HostResult {
    HostResult {
        outcome: "refused".into(),
        output: json!({"reason": reason}),
    }
}

impl Service {
    pub(in crate::playbooks) async fn plugin_dispatch(
        &self,
        owner: &RuntimeOwner,
        grant: Uuid,
        revision: i64,
        intent: &Intent,
        plugin_available: bool,
    ) -> Result<HostResult> {
        let name = command_plugin(&intent.command.name).ok_or(Error::Invalid)?;
        let (mut tx, stage, identity) = self.begin(Some(owner), grant, revision, intent).await?;
        match stage {
            Some(Stage::Retained(result)) => {
                tx.commit().await?;
                return Ok(result);
            }
            Some(Stage::Fresh) => {}
            _ => return Err(Error::Unknown),
        }
        let plugin = self.plugins.get(name).filter(|plugin| {
            plugin_available
                && plugin.registry().commands.get(&intent.command.name) == Some(&intent.command)
        });
        let mut events = Vec::new();
        let invocation = if let Some(plugin) = plugin {
            let remaining_ms = intent.deadline_at_ms - self.now()?;
            if remaining_ms <= 0 {
                return Err(Error::Unknown);
            }
            let idempotency_key = digest(&(self.namespace.engine_namespace(), &intent.id))?;
            plugin
                .invoke(Call {
                    command: &intent.command,
                    input: &intent.input,
                    idempotency_key: &idempotency_key,
                    project_id: self.namespace.project_id,
                    remaining_ms: remaining_ms as u64,
                })
                .await
        } else {
            Invocation::Unavailable
        };
        let (result, invalid) = match invocation {
            Invocation::Unknown => return Err(Error::Unknown),
            Invocation::Unavailable => (refused("plugin_unavailable"), false),
            Invocation::Invalid => (refused("invalid_plugin_response"), true),
            Invocation::Response(Response {
                outcome,
                output,
                events: produced,
            }) => {
                let plugin = plugin.ok_or(Error::Unknown)?;
                let now = self.now()?;
                let validated = produced
                    .into_iter()
                    .enumerate()
                    .map(|(index, event)| {
                        let envelope = plugin.event(&intent.id, index, event, now)?;
                        // The outbox caps the complete protected envelope at 64 KiB.
                        (canonical_bytes(&envelope).ok()?.len() <= 65_536).then_some(envelope)
                    })
                    .collect::<Option<Vec<_>>>();
                match validated {
                    Some(validated)
                        if self.validated(intent, &outcome, &output).is_ok()
                            && canonical_bytes(&output).is_ok_and(|bytes| {
                                bytes.len() <= intent.command.max_payload_bytes
                            }) =>
                    {
                        events = validated;
                        (HostResult { outcome, output }, false)
                    }
                    _ => (refused("invalid_plugin_response"), true),
                }
            }
        };
        self.validated(intent, &result.outcome, &result.output)?;
        self.record(
            &mut tx,
            grant,
            revision,
            intent,
            &identity,
            PLUGIN_COMPLETED,
            &serde_json::to_value(&result)?,
        )
        .await?;
        for event in events {
            let row_id = stable_id(
                "aq.playbook.plugin-event",
                self.namespace.namespace_id,
                self.namespace.generation,
                &event.event_id,
            )?;
            let at = event.observed_at_ms.ok_or(Error::Unknown)?;
            let retention = self.registry().events[&event.contract.name].retention_ms;
            let retain_until = at.saturating_add(i64::try_from(retention).unwrap_or(i64::MAX));
            self.seal_outbox(&mut tx, row_id, &event, at, Some(retain_until))
                .await?;
        }
        if invalid {
            self.queue_plugin_notice(&mut tx, name).await?;
        }
        self.commit_fresh(tx, grant, intent).await?;
        Ok(result)
    }

    async fn queue_plugin_notice(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        name: &str,
    ) -> Result<()> {
        let now = self.now()?;
        let date = DateTime::<Utc>::from_timestamp_millis(now)
            .ok_or(Error::Invalid)?
            .date_naive();
        let identity = format!("plugin:{name}:invalid:{date}");
        let id = stable_id("aq.playbook.plugin-notification", Uuid::nil(), 0, &identity)?;
        let request = aq_notifications::SendRequest {
            operation_id: id,
            message_id: id,
            revision: 1,
            scope: aq_lifecycle_api::Scope {
                project_id: self.namespace.project_id,
                workspace_id: self.namespace.workspace_id,
            },
            recipient_user_id: self.namespace.owner_user_id,
            summary: format!("Playbook plugin {name} returned an invalid response."),
            ttl_ms: 86_400_000,
        };
        sqlx::query("INSERT INTO playbook_plugin_notifications(identity,namespace_id,generation,request,created_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(identity) DO NOTHING")
            .bind(identity).bind(self.namespace.namespace_id).bind(self.namespace.generation)
            .bind(serde_json::to_value(request)?).bind(now).execute(&mut **tx).await?;
        Ok(())
    }

    pub(in crate::playbooks) async fn flush_plugin_notices(
        &self,
        owner: &RuntimeOwner,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows: Vec<(String, Value)> = sqlx::query_as("SELECT identity,request FROM playbook_plugin_notifications WHERE namespace_id=$1 AND generation=$2 AND NOT submitted ORDER BY created_at_ms LIMIT 2")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for (identity, request) in rows {
            self.notifications
                .enqueue(
                    self.namespace.owner_user_id,
                    serde_json::from_value(request)?,
                )
                .await
                .map_err(|_| Error::Overloaded)?;
            sqlx::query(
                "UPDATE playbook_plugin_notifications SET submitted=true WHERE identity=$1",
            )
            .bind(identity)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}
