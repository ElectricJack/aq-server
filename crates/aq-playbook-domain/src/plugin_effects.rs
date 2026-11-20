//! Validate plugin output and manage retained notices for invalid responses.
use crate::{Error, Namespace, Result, digest, host_values::HostResult, stable_id};
use aq_playbook_host::{
    model::{EventEnvelope, canonical_bytes},
    plugins::{Call, Invocation, Plugin, Response},
    runner::Intent,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

fn refused(reason: &str) -> HostResult {
    HostResult {
        outcome: "refused".into(),
        output: json!({"reason": reason}),
    }
}

pub struct PluginEffect {
    pub result: HostResult,
    pub events: Vec<EventEnvelope>,
    pub invalid: bool,
}

pub async fn invoke(
    plugin: Option<&Plugin>,
    namespace: &Namespace,
    intent: &Intent,
    now: impl Fn() -> Result<i64>,
) -> Result<PluginEffect> {
    let mut events = Vec::new();
    let invocation = if let Some(plugin) = plugin {
        let remaining_ms = intent.deadline_at_ms - now()?;
        if remaining_ms <= 0 {
            return Err(Error::Unknown);
        }
        let idempotency_key = digest(&(namespace.engine_namespace(), &intent.id))?;
        plugin
            .invoke(Call {
                command: &intent.command,
                input: &intent.input,
                idempotency_key: &idempotency_key,
                project_id: namespace.project_id,
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
            let now = now()?;
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
                    if intent
                        .command
                        .outcomes
                        .get(&outcome)
                        .is_some_and(|schema| schema.validate(&output).is_ok())
                        && canonical_bytes(&output)
                            .is_ok_and(|bytes| bytes.len() <= intent.command.max_payload_bytes) =>
                {
                    events = validated;
                    (HostResult { outcome, output }, false)
                }
                _ => (refused("invalid_plugin_response"), true),
            }
        }
    };
    Ok(PluginEffect {
        result,
        events,
        invalid,
    })
}

pub fn invalid_notice(name: &str, now: i64) -> Result<(String, Uuid)> {
    let date = DateTime::<Utc>::from_timestamp_millis(now)
        .ok_or(Error::Invalid)?
        .date_naive();
    let identity = format!("plugin:{name}:invalid:{date}");
    let id = stable_id("aq.playbook.plugin-notification", Uuid::nil(), 0, &identity)?;
    Ok((identity, id))
}

pub struct PluginNotices<'a>(pub &'a Namespace);

impl PluginNotices<'_> {
    pub async fn retain(
        &self,
        db: &mut PgConnection,
        identity: &str,
        request: Value,
        now: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO playbook_plugin_notifications(identity,namespace_id,generation,request,created_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(identity) DO NOTHING")
            .bind(identity).bind(self.0.namespace_id).bind(self.0.generation)
            .bind(request).bind(now).execute(db).await?;
        Ok(())
    }

    pub async fn due(&self, db: &mut PgConnection) -> Result<Vec<(String, Value)>> {
        Ok(sqlx::query_as("SELECT identity,request FROM playbook_plugin_notifications WHERE namespace_id=$1 AND generation=$2 AND NOT submitted ORDER BY created_at_ms LIMIT 2")
            .bind(self.0.namespace_id).bind(self.0.generation).fetch_all(db).await?)
    }

    pub async fn submitted(&self, db: impl PgExecutor<'_>, identity: &str) -> Result<()> {
        sqlx::query("UPDATE playbook_plugin_notifications SET submitted=true WHERE identity=$1")
            .bind(identity)
            .execute(db)
            .await?;
        Ok(())
    }
}
