//! The retained host effect is the commit point for the plugin result and events.
use super::{PLUGIN_COMPLETED, Stage, *};
use aq_playbook_domain::plugin_effects::{self, PluginEffect, PluginNotices};
use aq_playbook_host::plugins::command_plugin;

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
        let PluginEffect {
            result,
            events,
            invalid,
        } = plugin_effects::invoke(plugin, &self.namespace, intent, || self.now()).await?;
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
        self.commit_fresh(tx, grant, intent)
            .await
            .map_err(|error| {
                // A child may already have applied the effect if the deadline or
                // grant expired during the call, even when AQ refused the commit.
                if error == Error::Forbidden {
                    Error::Unknown
                } else {
                    error
                }
            })?;
        Ok(result)
    }

    async fn queue_plugin_notice(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        name: &str,
    ) -> Result<()> {
        let now = self.now()?;
        let (identity, id) = plugin_effects::invalid_notice(name, now)?;
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
        PluginNotices(&self.namespace)
            .retain(tx, &identity, serde_json::to_value(request)?, now)
            .await
    }

    pub(in crate::playbooks) async fn flush_plugin_notices(
        &self,
        owner: &RuntimeOwner,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let notices = PluginNotices(&self.namespace);
        let rows = notices.due(&mut tx).await?;
        tx.commit().await?;
        for (identity, request) in rows {
            self.notifications
                .enqueue(
                    self.namespace.owner_user_id,
                    serde_json::from_value(request)?,
                )
                .await
                .map_err(|_| Error::Overloaded)?;
            notices.submitted(&self.pool, &identity).await?;
        }
        Ok(())
    }
}
