use super::*;
use sqlx::postgres::PgRow;

async fn ready_nodes(state: &crate::AppState, scope: &Scope) -> Result<Vec<Value>> {
    let graph = state.agent_access.as_ref().ok_or(Error::Unavailable)?;
    let mut ready = Vec::new();
    let mut cursor = None;
    // Bound work to the episode capacity. A partial projection is
    // retried without acknowledging the source or resolving gates.
    for _ in 0..64 {
        let page = graph
            .scheduling_ready_current(
                scope.workspace_id,
                64,
                cursor.as_deref(),
                state.clock.as_ref(),
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        ready.extend(page.items.into_iter().map(|r| r.node));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if cursor.is_some() {
        return Err(Error::Overloaded);
    }
    Ok(ready)
}

impl Policy {
    /// Consume the D3 outbox independently of engine delivery. Freeze the
    /// notification projection before observing it; a crash retries exact IDs.
    pub async fn reconcile_graph(&self, state: &crate::AppState) -> Result<usize> {
        let rows = sqlx::query("SELECT o.event_id,o.notification_events,b.binding FROM agent_graph_outbox o JOIN agent_graph_operations g USING(operation_id) JOIN agent_launch_bindings a ON a.attempt_id=g.attempt_id JOIN project_execution_bindings b ON b.project_id=(a.binding->>'project_id')::uuid AND b.binding->>'graph_workspace_id'=a.binding->>'workspace_id' WHERE NOT o.notification_observed AND g.phase='confirmed' AND b.binding->>'enabled'='true' ORDER BY g.operation_seq LIMIT 1")
            .fetch_all(&self.service().pool).await?;
        for row in &rows {
            self.observe_graph_row(state, row).await?;
        }
        Ok(rows.len())
    }

    async fn observe_graph_row(&self, state: &crate::AppState, row: &PgRow) -> Result<()> {
        let id: Uuid = row.get("event_id");
        let events: Vec<Event> = match row.get::<Option<Value>, _>("notification_events") {
            Some(frozen) => serde_json::from_value(frozen)?,
            None => {
                let binding: Value = row.get("binding");
                let scope = Scope {
                    project_id: uuid(&binding["project_id"])?,
                    workspace_id: uuid(&binding["graph_workspace_id"])?,
                };
                self.freeze_gates(state, Origin::new(id, scope)).await?
            }
        };
        for event in events {
            self.observe(event).await?;
        }
        // Result attention is reconciled too: graph delivery never claims
        // that a failed process completed, or that a held task was released.
        self.reconcile_tasks().await?;
        sqlx::query("UPDATE agent_graph_outbox SET notification_observed=TRUE WHERE event_id=$1")
            .bind(id)
            .execute(&self.service().pool)
            .await?;
        Ok(())
    }
    async fn freeze_gates(&self, state: &crate::AppState, origin: Origin) -> Result<Vec<Event>> {
        let Origin { id, scope } = &origin;
        let ready = ready_nodes(state, scope).await?;
        let events = milestone_events(*id, scope.clone(), &ready)?;
        let open: Vec<String> = sqlx::query_scalar("SELECT subject FROM notification_episodes WHERE kind='milestone.gate_ready' AND project_id=$1 AND workspace_id=$2 AND open ORDER BY subject LIMIT 4096")
                        .bind(scope.project_id).bind(scope.workspace_id).fetch_all(&self.service().pool).await?;
        let events = origin.gate_transitions(events, open);
        let frozen: Value = sqlx::query_scalar("UPDATE agent_graph_outbox SET notification_events=COALESCE(notification_events,$2) WHERE event_id=$1 RETURNING notification_events")
                    .bind(id).bind(serde_json::to_value(&events)?).fetch_one(&self.service().pool).await?;
        Ok(serde_json::from_value(frozen)?)
    }
}
