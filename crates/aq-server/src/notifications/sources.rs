//! Automatic producers run on the notification pool, outside control-owner
//! transactions. Durable owner rows are observations, never execution permits.
use super::{
    Error, Result,
    policy::{Event, Kind, MILESTONE_GATE_LABEL, Policy},
};
use aq_lifecycle_api::Scope;
pub use aq_notifications::policy::Origin;
pub struct Preservation {
    pub node: Uuid,
    pub attempt: Uuid,
    pub bytes: u64,
    pub expires_at_ms: u64,
}

use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

impl Policy {
    /// C6b integration port. The profile, rather than the failed attempt, keys
    /// the episode. Producers must supply the retained failure event identity.
    pub async fn observe_failure_reason(
        &self,
        origin: Origin,
        profile: &str,
        reason: &str,
    ) -> Result<Option<Value>> {
        if origin.id.is_nil() {
            return Err(Error::Invalid);
        }
        let Some(kind) = Kind::from_failure_reason(reason) else {
            return Ok(None);
        };
        self.observe(origin.event(kind, profile.to_string(), true))
            .await
            .map(Some)
    }

    /// F5 integration port; never copy question text into a notification.
    pub async fn observe_question(
        &self,
        origin: Origin,
        decision: Uuid,
        pending: bool,
    ) -> Result<Value> {
        self.observe(origin.identified(Kind::QuestionAsked, decision, pending)?)
            .await
    }

    async fn changed(
        &self,
        kind: Kind,
        (scope, subject): (Scope, String),
        open: bool,
    ) -> Result<()> {
        let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notification_episodes WHERE kind=$1 AND project_id=$2 AND workspace_id=$3 AND subject=$4 AND open)")
            .bind(kind.as_str()).bind(scope.project_id).bind(scope.workspace_id).bind(&subject)
            .fetch_one(&self.service().pool).await?;
        if current != open {
            self.observe(Origin::new(Uuid::new_v4(), scope).event(kind, subject, open))
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
                (scope(row), row.get::<Uuid, _>("decision_id").to_string()),
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
        let rows = sqlx::query("SELECT n.id,b.project_id,(b.binding->>'graph_workspace_id')::uuid AS workspace_id,(n.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM worker_settings s WHERE s.kind='node' AND s.removed AND s.id=n.id::text) AND (n.last_seen_at IS NULL OR n.last_seen_at<to_timestamp($1::double precision/1000))) AS offline FROM nodes n CROSS JOIN project_execution_bindings b LEFT JOIN notification_episodes e ON e.kind='worker.offline' AND e.project_id=b.project_id AND e.workspace_id=(b.binding->>'graph_workspace_id')::uuid AND e.subject=n.id::text WHERE b.binding->>'enabled'='true' AND (COALESCE((b.binding->>'allow_enrolled_nodes')::boolean,TRUE) OR COALESCE(b.binding->'allowed_node_ids','[]') ? n.id::text OR EXISTS(SELECT 1 FROM jsonb_array_elements_text(COALESCE(b.binding->'allowed_worker_groups','[]')) g WHERE COALESCE($2->g.value,'[]') ? n.id::text)) AND COALESCE(e.open,FALSE) IS DISTINCT FROM (n.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM worker_settings s WHERE s.kind='node' AND s.removed AND s.id=n.id::text) AND (n.last_seen_at IS NULL OR n.last_seen_at<to_timestamp($1::double precision/1000))) ORDER BY b.project_id,n.id LIMIT 32")
            .bind(threshold).bind(groups).fetch_all(&self.service().pool).await?;
        for row in &rows {
            self.changed(
                Kind::WorkerOffline,
                (scope(row), row.get::<Uuid, _>("id").to_string()),
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
            let mut e =
                Origin::new(Uuid::new_v4(), scope(row)).event(Kind::StoragePressure, subject, open);
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
        retained: Preservation,
    ) -> Result<Option<Value>> {
        if source.is_nil() || retained.attempt.is_nil() {
            return Err(Error::Invalid);
        }
        let ran: Option<Uuid> =
            sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
                .bind(retained.attempt)
                .fetch_optional(&self.service().pool)
                .await?;
        if ran != Some(retained.node) {
            return Err(Error::NotFound);
        }
        let Some(row) = sqlx::query("SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid AS project_id,(l.execution_spec#>>'{task,workspace_id}')::uuid AS workspace_id FROM profiled_launches l JOIN project_execution_bindings b ON b.project_id=(l.execution_spec#>>'{lifecycle,project_id}')::uuid AND b.binding->>'enabled'='true' AND b.binding->>'graph_workspace_id'=l.execution_spec#>>'{task,workspace_id}' WHERE l.attempt_id=$1")
            .bind(retained.attempt).fetch_optional(&self.service().pool).await?
        else {
            return Ok(None);
        };
        self.observe_preservation(Origin::new(source, scope(&row)), retained)
            .await
            .map(Some)
    }

    /// The scoped half of [`Self::observe_preserved`]: one opened episode per
    /// attempt; the title carries the node, bytes held and the expiry.
    pub async fn observe_preservation(
        &self,
        origin: Origin,
        retained: Preservation,
    ) -> Result<Value> {
        let Preservation {
            node,
            attempt,
            bytes: preserved_bytes,
            expires_at_ms,
        } = retained;
        let mut e = origin.identified(Kind::SandboxPreserved, attempt, true)?;
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
        origin: Origin,
        attempt: Uuid,
        title: String,
    ) -> Result<Value> {
        let mut e = origin.identified(Kind::SessionStalled, attempt, true)?;
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
                (scope(row), row.get("subject")),
                row.get("attention"),
            )
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
            Ok(Origin::new(source, scope.clone()).event(
                Kind::MilestoneGateReady,
                uuid(&n["id"])?.to_string(),
                true,
            ))
        })
        .collect()
}

mod graph;
