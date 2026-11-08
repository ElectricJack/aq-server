//! Automatic producers run on the notification pool, outside control-owner
//! transactions. Durable owner rows are observations, never execution permits.
use super::{
    Error, Result,
    policy::{Event, Kind, MILESTONE_GATE_LABEL, Policy, Transition, derived_id},
};
use aq_lifecycle_api::Scope;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

impl Policy {
    /// C6b integration port. The profile, rather than the failed attempt, keys
    /// the episode. Producers must supply the retained failure event identity.
    pub async fn observe_failure_reason(
        &self,
        source: Uuid,
        scope: Scope,
        profile: &str,
        reason: &str,
    ) -> Result<Option<Value>> {
        if source.is_nil() {
            return Err(Error::Invalid);
        }
        let Some(kind) = Kind::from_failure_reason(reason) else {
            return Ok(None);
        };
        self.observe(event(source, kind, scope, profile.to_string(), true))
            .await
            .map(Some)
    }

    /// F5 integration port; never copy question text into a notification.
    pub async fn observe_question(
        &self,
        source: Uuid,
        scope: Scope,
        decision: Uuid,
        pending: bool,
    ) -> Result<Value> {
        if source.is_nil() || decision.is_nil() {
            return Err(Error::Invalid);
        }
        self.observe(event(
            source,
            Kind::QuestionAsked,
            scope,
            decision.to_string(),
            pending,
        ))
        .await
    }

    async fn changed(&self, kind: Kind, scope: Scope, subject: String, open: bool) -> Result<()> {
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notification_episodes WHERE kind=$1 AND project_id=$2 AND workspace_id=$3 AND subject=$4 AND open)")
            .bind(kind.as_str()).bind(scope.project_id).bind(scope.workspace_id).bind(&subject)
            .fetch_one(&self.service().pool).await?;
        if current != open {
            self.observe(event(Uuid::new_v4(), kind, scope, subject, open))
                .await?;
        }
        Ok(())
    }

    /// Pending requests need the owner. Answer/cancel/expiry resolves the same
    /// episode. SQL observes only public identities, never encrypted review text.
    pub async fn reconcile_reviews(&self) -> Result<usize> {
        let rows = sqlx::query("SELECT d.decision_id,s.project_id,s.workspace_id,(d.state='pending' AND d.expires_at_ms>$1) AS pending FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) JOIN project_execution_bindings b ON b.project_id=s.project_id AND b.binding->>'enabled'='true' AND b.binding->>'graph_workspace_id'=s.workspace_id::text LEFT JOIN notification_episodes e ON e.kind='decision.needed' AND e.project_id=s.project_id AND e.workspace_id=s.workspace_id AND e.subject=d.decision_id::text WHERE COALESCE(e.open,FALSE) IS DISTINCT FROM (d.state='pending' AND d.expires_at_ms>$1) ORDER BY d.decision_id LIMIT 32")
            .bind(self.service().now()? as i64).fetch_all(&self.service().pool).await?;
        for row in &rows {
            self.changed(
                Kind::DecisionNeeded,
                scope(row),
                row.get::<Uuid, _>("decision_id").to_string(),
                row.get("pending"),
            )
            .await?;
        }
        Ok(rows.len())
    }

    /// Same heartbeat threshold as /api/workers. Only workers eligible in an
    /// enabled project are reported; revocation resolves a retained episode.
    pub async fn reconcile_workers(&self, offline_after_secs: u64, groups: Value) -> Result<usize> {
        let threshold = self
            .service()
            .now()?
            .saturating_sub(offline_after_secs.saturating_mul(1000)) as i64;
        let rows = sqlx::query("SELECT n.id,b.project_id,(b.binding->>'graph_workspace_id')::uuid AS workspace_id,(n.revoked_at IS NULL AND (n.last_seen_at IS NULL OR n.last_seen_at<to_timestamp($1::double precision/1000))) AS offline FROM nodes n CROSS JOIN project_execution_bindings b LEFT JOIN notification_episodes e ON e.kind='worker.offline' AND e.project_id=b.project_id AND e.workspace_id=(b.binding->>'graph_workspace_id')::uuid AND e.subject=n.id::text WHERE b.binding->>'enabled'='true' AND (COALESCE((b.binding->>'allow_enrolled_nodes')::boolean,TRUE) OR COALESCE(b.binding->'allowed_node_ids','[]') ? n.id::text OR EXISTS(SELECT 1 FROM jsonb_array_elements_text(COALESCE(b.binding->'allowed_worker_groups','[]')) g WHERE COALESCE($2->g.value,'[]') ? n.id::text)) AND COALESCE(e.open,FALSE) IS DISTINCT FROM (n.revoked_at IS NULL AND (n.last_seen_at IS NULL OR n.last_seen_at<to_timestamp($1::double precision/1000))) ORDER BY b.project_id,n.id LIMIT 32")
            .bind(threshold).bind(groups).fetch_all(&self.service().pool).await?;
        for row in &rows {
            self.changed(
                Kind::WorkerOffline,
                scope(row),
                row.get::<Uuid, _>("id").to_string(),
                row.get("offline"),
            )
            .await?;
        }
        Ok(rows.len())
    }

    /// SBX25 storage pressure with hysteresis: an episode opens once usage
    /// reaches 80% and stays open until usage drops below 75%. A node subject
    /// is reported to the enabled projects it is eligible for; `central` to
    /// every enabled project.
    pub async fn reconcile_storage(&self, groups: Value) -> Result<usize> {
        use crate::node_storage::{OPEN_AT_BPS, RESOLVE_BELOW_BPS};
        let rows = sqlx::query("WITH s AS (SELECT r.subject,r.used_bps,b.project_id,(b.binding->>'graph_workspace_id')::uuid AS workspace_id FROM node_storage_reports r CROSS JOIN project_execution_bindings b WHERE b.binding->>'enabled'='true' AND (r.node_id IS NULL OR COALESCE((b.binding->>'allow_enrolled_nodes')::boolean,TRUE) OR COALESCE(b.binding->'allowed_node_ids','[]') ? r.subject OR EXISTS(SELECT 1 FROM jsonb_array_elements_text(COALESCE(b.binding->'allowed_worker_groups','[]')) g WHERE COALESCE($1->g.value,'[]') ? r.subject))), d AS (SELECT s.*,CASE WHEN COALESCE(e.open,FALSE) THEN s.used_bps>=$3 ELSE s.used_bps>=$2 END AS pressure,COALESCE(e.open,FALSE) AS current FROM s LEFT JOIN notification_episodes e ON e.kind='storage.pressure' AND e.project_id=s.project_id AND e.workspace_id=s.workspace_id AND e.subject=s.subject) SELECT * FROM d WHERE current IS DISTINCT FROM pressure ORDER BY project_id,subject LIMIT 32")
            .bind(groups).bind(OPEN_AT_BPS).bind(RESOLVE_BELOW_BPS)
            .fetch_all(&self.service().pool).await?;
        for row in &rows {
            let subject: String = row.get("subject");
            let used: i32 = row.get("used_bps");
            let open: bool = row.get("pressure");
            let mut e = event(
                Uuid::new_v4(),
                Kind::StoragePressure,
                scope(row),
                subject,
                open,
            );
            e.title = format!("Disk {}.{:02}% used", used / 100, used % 100);
            self.observe(e).await?;
        }
        Ok(rows.len())
    }

    /// SBX22(a) node preserve event. `source` is the node's preserve event ID,
    /// so a replayed event never produces a second notification. The attempt
    /// must have run on `node`; attempts outside an enabled project are
    /// recorded nowhere and return `None`.
    pub async fn observe_preserved(
        &self,
        source: Uuid,
        node: Uuid,
        attempt: Uuid,
        preserved_bytes: u64,
        expires_at_ms: u64,
    ) -> Result<Option<Value>> {
        if source.is_nil() || attempt.is_nil() {
            return Err(Error::Invalid);
        }
        let ran: Option<Uuid> =
            sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_optional(&self.service().pool)
                .await?;
        if ran != Some(node) {
            return Err(Error::NotFound);
        }
        let Some(row) = sqlx::query("SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid AS project_id,(l.execution_spec#>>'{task,workspace_id}')::uuid AS workspace_id FROM profiled_launches l JOIN project_execution_bindings b ON b.project_id=(l.execution_spec#>>'{lifecycle,project_id}')::uuid AND b.binding->>'enabled'='true' AND b.binding->>'graph_workspace_id'=l.execution_spec#>>'{task,workspace_id}' WHERE l.attempt_id=$1")
            .bind(attempt).fetch_optional(&self.service().pool).await?
        else {
            return Ok(None);
        };
        self.observe_preservation(
            source,
            scope(&row),
            node,
            attempt,
            preserved_bytes,
            expires_at_ms,
        )
        .await
        .map(Some)
    }

    /// The scoped half of [`Self::observe_preserved`]: one opened episode per
    /// attempt; the title carries the node, bytes held and the expiry.
    pub async fn observe_preservation(
        &self,
        source: Uuid,
        scope: Scope,
        node: Uuid,
        attempt: Uuid,
        preserved_bytes: u64,
        expires_at_ms: u64,
    ) -> Result<Value> {
        if source.is_nil() || attempt.is_nil() {
            return Err(Error::Invalid);
        }
        let mut e = event(
            source,
            Kind::SandboxPreserved,
            scope,
            attempt.to_string(),
            true,
        );
        let expiry = i64::try_from(expires_at_ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .ok_or(Error::Invalid)?;
        e.title = format!(
            "Node {node} holds {} until {}",
            bytes(preserved_bytes),
            expiry.format("%Y-%m-%d %H:%M UTC")
        );
        self.observe(e).await
    }

    /// H3 rung 3: one `session.stalled` episode per stalled attempt. The
    /// source is the ladder action ID, so a retried notify is idempotent.
    pub async fn observe_stall(
        &self,
        source: Uuid,
        scope: Scope,
        attempt: Uuid,
        title: String,
    ) -> Result<Value> {
        if source.is_nil() || attempt.is_nil() {
            return Err(Error::Invalid);
        }
        let mut e = event(
            source,
            Kind::SessionStalled,
            scope,
            attempt.to_string(),
            true,
        );
        e.title = title;
        self.observe(e).await
    }

    /// Failure claims and held/rework default-policy settlements need attention.
    /// This never promotes a process outcome to a Graph task-state transition.
    /// Latest retained result across attempts wins, so later success resolves.
    pub async fn reconcile_tasks(&self) -> Result<usize> {
        let rows = sqlx::query("WITH latest AS (SELECT DISTINCT ON (l.execution_spec#>>'{lifecycle,project_id}',l.execution_spec#>>'{task,task_id}') (l.execution_spec#>>'{lifecycle,project_id}')::uuid AS project_id,(l.execution_spec#>>'{task,workspace_id}')::uuid AS workspace_id,l.execution_spec#>>'{task,task_id}' AS subject,(convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome'='failed' OR COALESCE(a.disposition IN ('held','rework_required'),FALSE)) AS attention FROM profiled_result_heads h JOIN profiled_result_reports r USING(report_id) JOIN profiled_launches l ON l.attempt_id=r.attempt_id LEFT JOIN profiled_auto_settlements a ON a.report_id=r.report_id ORDER BY l.execution_spec#>>'{lifecycle,project_id}',l.execution_spec#>>'{task,task_id}',(convert_from(r.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint DESC,r.report_id DESC) SELECT l.* FROM latest l JOIN project_execution_bindings b ON b.project_id=l.project_id AND b.binding->>'enabled'='true' AND b.binding->>'graph_workspace_id'=l.workspace_id::text LEFT JOIN notification_episodes e ON e.kind='task.failed_or_blocked' AND e.project_id=l.project_id AND e.workspace_id=l.workspace_id AND e.subject=l.subject WHERE COALESCE(e.open,FALSE) IS DISTINCT FROM l.attention ORDER BY l.project_id,l.subject LIMIT 32")
            .fetch_all(&self.service().pool).await?;
        for row in &rows {
            self.changed(
                Kind::TaskFailedOrBlocked,
                scope(row),
                row.get("subject"),
                row.get("attention"),
            )
            .await?;
        }
        Ok(rows.len())
    }

    /// Consume the D3 outbox independently of engine delivery. Freeze the
    /// notification projection before observing it; a crash retries exact IDs.
    pub async fn reconcile_graph(&self, state: &crate::AppState) -> Result<usize> {
        let rows = sqlx::query("SELECT o.event_id,o.notification_events,b.binding FROM agent_graph_outbox o JOIN agent_graph_operations g USING(operation_id) JOIN agent_launch_bindings a ON a.attempt_id=g.attempt_id JOIN project_execution_bindings b ON b.project_id=(a.binding->>'project_id')::uuid AND b.binding->>'graph_workspace_id'=a.binding->>'workspace_id' WHERE NOT o.notification_observed AND g.phase='confirmed' AND b.binding->>'enabled'='true' ORDER BY g.operation_seq LIMIT 1")
            .fetch_all(&self.service().pool).await?;
        for row in &rows {
            let id: Uuid = row.get("event_id");
            let frozen: Option<Value> = row.get("notification_events");
            let events: Vec<Event> = if let Some(frozen) = frozen {
                serde_json::from_value(frozen)?
            } else {
                let binding: Value = row.get("binding");
                let scope = Scope {
                    project_id: uuid(&binding["project_id"])?,
                    workspace_id: uuid(&binding["graph_workspace_id"])?,
                };
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
                let mut events = milestone_events(id, scope.clone(), &ready)?;
                let open: Vec<String> = sqlx::query_scalar("SELECT subject FROM notification_episodes WHERE kind='milestone.gate_ready' AND project_id=$1 AND workspace_id=$2 AND open ORDER BY subject LIMIT 4096")
                        .bind(scope.project_id).bind(scope.workspace_id).fetch_all(&self.service().pool).await?;
                let ready_subjects: std::collections::HashSet<_> =
                    events.iter().map(|e| e.subject.clone()).collect();
                // Unchanged ready gates need no new policy receipt on every
                // Graph operation. Only transitions consume retention capacity.
                events.retain(|e| !open.contains(&e.subject));
                for subject in open {
                    if !ready_subjects.contains(&subject) {
                        events.push(event(
                            id,
                            Kind::MilestoneGateReady,
                            scope.clone(),
                            subject,
                            false,
                        ));
                    }
                }
                let frozen: Value = sqlx::query_scalar("UPDATE agent_graph_outbox SET notification_events=COALESCE(notification_events,$2) WHERE event_id=$1 RETURNING notification_events")
                    .bind(id).bind(serde_json::to_value(&events)?).fetch_one(&self.service().pool).await?;
                serde_json::from_value(frozen)?
            };
            for event in events {
                self.observe(event).await?;
            }
            // Result attention is reconciled too: graph delivery never claims
            // that a failed process completed, or that a held task was released.
            self.reconcile_tasks().await?;
            sqlx::query(
                "UPDATE agent_graph_outbox SET notification_observed=TRUE WHERE event_id=$1",
            )
            .bind(id)
            .execute(&self.service().pool)
            .await?;
        }
        Ok(rows.len())
    }
}

fn scope(row: &sqlx::postgres::PgRow) -> Scope {
    Scope {
        project_id: row.get("project_id"),
        workspace_id: row.get("workspace_id"),
    }
}
fn bytes(n: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    match n as f64 {
        v if v >= GIB => format!("{:.1} GiB", v / GIB),
        v if v >= MIB => format!("{:.1} MiB", v / MIB),
        _ => format!("{n} bytes"),
    }
}
fn uuid(value: &Value) -> Result<Uuid> {
    value
        .as_str()
        .and_then(|s| s.parse().ok())
        .filter(|v: &Uuid| !v.is_nil())
        .ok_or(Error::Invalid)
}
fn event(source: Uuid, kind: Kind, scope: Scope, subject: String, open: bool) -> Event {
    Event {
        source_event_id: derived_id(&[
            "aq-source/1",
            &source.to_string(),
            kind.as_str(),
            &scope.project_id.to_string(),
            &scope.workspace_id.to_string(),
            &subject,
            if open { "opened" } else { "resolved" },
        ]),
        kind,
        subject,
        scope,
        transition: if open {
            Transition::Opened
        } else {
            Transition::Resolved
        },
        title: "Open AQ to review".into(),
    }
}

/// Nodes come from Graph's validated ready projection, not caller-supplied
/// labels alone. Keep this pure adapter available for fixture qualification.
pub fn milestone_events(source: Uuid, scope: Scope, ready_nodes: &[Value]) -> Result<Vec<Event>> {
    if source.is_nil() {
        return Err(Error::Invalid);
    }
    ready_nodes
        .iter()
        .filter(|n| {
            n["labels"]
                .as_array()
                .is_some_and(|labels| labels.contains(&json!(MILESTONE_GATE_LABEL)))
        })
        .map(|n| {
            Ok(event(
                source,
                Kind::MilestoneGateReady,
                scope.clone(),
                uuid(&n["id"])?.to_string(),
                true,
            ))
        })
        .collect()
}
