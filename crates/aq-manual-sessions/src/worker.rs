use super::*;
mod scheduler;
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
use scheduler::Scheduler;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    max_slots: u32,
    repositories: Vec<Uuid>,
    #[serde(default = "legacy_providers")]
    providers: Vec<String>,
    #[serde(default)]
    busy: bool,
    #[serde(default)]
    reports: Vec<Report>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    id: Uuid,
    status: String,
    stage: String,
    #[serde(default)]
    snapshot: Option<String>,
    #[serde(default)]
    acknowledged: Vec<Uuid>,
}
fn legacy_providers() -> Vec<String> {
    vec!["anthropic".into()]
}
pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/worker/manual-agents", post(poll))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
}
async fn poll(
    State(s): State<Service>,
    headers: HeaderMap,
    Json(v): Json<Poll>,
) -> Result<Json<Value>> {
    let node = (s.node)(headers.clone()).await?;
    v.validate()?;
    let at = now();
    let mut tx = s.pool.begin().await?;
    aq_machine_control::management::lock(&mut tx)
        .await
        .map_err(management::storage)?;
    // A single node lock orders start/slot mutations and poll claims.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,107))")
        .bind(node.to_string())
        .execute(&mut *tx)
        .await?;
    let active: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE")
            .bind(node)
            .fetch_optional(&mut *tx)
            .await?;
    if active.is_none() || (s.node)(headers).await? != node {
        return Err(Error(StatusCode::UNAUTHORIZED, "Worker enrollment changed"));
    }
    let desired:Option<String>=sqlx::query_scalar("SELECT c.desired FROM machine_controls c JOIN nodes n ON n.machine_key=c.machine_key WHERE n.id=$1").bind(node).fetch_optional(&mut *tx).await?;
    let limits = aq_machine_control::management::capacity(&mut tx, node)
        .await
        .map_err(management::storage)?;
    if desired.as_deref() == Some("stopped") || limits.removed {
        sqlx::query("UPDATE manual_agent_sessions SET desired_running=false WHERE node_id=$1 AND stopped_at_ms IS NULL").bind(node).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO manual_worker_reports(node_id,max_slots,repositories,observed_at_ms,providers) VALUES($1,$2,$3,$4,$5) ON CONFLICT(node_id) DO UPDATE SET max_slots=EXCLUDED.max_slots,repositories=EXCLUDED.repositories,observed_at_ms=EXCLUDED.observed_at_ms,providers=EXCLUDED.providers").bind(node).bind(v.max_slots as i32).bind(json!(v.repositories)).bind(at).bind(json!(v.providers)).execute(&mut *tx).await?;
    let owned = update_reports(&v.reports, &mut tx, node, at).await?;
    // A stopped/disabled project revokes the session's rolling lease.
    sqlx::query("UPDATE manual_agent_sessions m SET desired_running=false WHERE node_id=$1 AND stopped_at_ms IS NULL AND NOT EXISTS(SELECT 1 FROM project_execution_bindings p WHERE p.project_id=m.project_id AND p.binding->>'enabled'='true')").bind(node).execute(&mut *tx).await?;
    let raw:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(m)-'actor_id'-'stopped_at_ms' FROM manual_agent_sessions m WHERE node_id=$1 AND stopped_at_ms IS NULL ORDER BY created_at_ms,id LIMIT 32").bind(node).fetch_all(&mut *tx).await?;
    let mut sessions: Vec<Session> = raw
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| bad("Invalid retained session"))?;
    let slots: Vec<(Uuid, i32)> =
        sqlx::query_as("SELECT project_id,slots FROM manual_project_slots WHERE node_id=$1")
            .bind(node)
            .fetch_all(&mut *tx)
            .await?;
    let mut scheduler = Scheduler::new(&sessions, slots, &v, desired.as_deref());
    if let Some(witness) = s.compute_witness {
        witness(&mut tx, node).await?;
    }
    let mut jobs = Vec::new();
    for x in &mut sessions {
        if !scheduler.includes(&mut tx, x, (node, at)).await? {
            continue;
        }
        jobs.push(x.clone());
    }
    tx.commit().await?;
    let commands = commands(&s, owned, &jobs, at).await?;
    Ok(Json(
        json!({"sessions":jobs,"commands":commands,"pending_count":sessions.iter().filter(|s|s.desired_running).count(),"lease_ms":45000,"server_time_ms":at}),
    ))
}

impl Poll {
    fn validate(&self) -> Result<()> {
        let valid_limits = (1..=8).contains(&self.max_slots)
            && self.repositories.len() <= 16
            && self.reports.len() <= 32;
        let providers = self.providers.len() <= 2
            && self
                .providers
                .iter()
                .all(|p| matches!(p.as_str(), "anthropic" | "openai"));
        if !valid_limits || !providers {
            return Err(bad("Invalid manual worker bounds"));
        }
        if self.reports.iter().any(|r| !r.valid()) {
            return Err(bad("Invalid session report"));
        }
        Ok(())
    }
}
impl Report {
    fn valid(&self) -> bool {
        let valid_status = matches!(
            self.status.as_str(),
            "preparing" | "running" | "stopping" | "stopped" | "failed"
        );
        valid_status
            && self.stage.len() <= 128
            && self.snapshot.as_ref().is_none_or(|s| s.len() <= 131072)
            && self.acknowledged.len() <= 128
    }
    fn apply(&self, screen: &mut terminal::Screen, at: i64) {
        screen.seen = at;
        if let Some(text) = self
            .snapshot
            .as_ref()
            .filter(|text| screen.sequence == 0 || screen.text.as_str() != text.as_str())
        {
            screen.text = text.clone();
            screen.sequence += 1;
        }
        for id in &self.acknowledged {
            if let Some(command) = screen.commands.get_mut(id) {
                command.acknowledge(at);
            }
        }
    }
}
async fn update_reports<'a>(
    reports: &'a [Report],
    conn: &mut sqlx::PgConnection,
    node: Uuid,
    at: i64,
) -> Result<Vec<&'a Report>> {
    let mut owned = Vec::new();
    for r in reports {
        let changed=sqlx::query("UPDATE manual_agent_sessions SET status=$3,stage=$4,updated_at_ms=CASE WHEN status<>$3 OR stage<>$4 THEN $5 ELSE updated_at_ms END,worker_seen_at_ms=$5,stopped_at_ms=CASE WHEN $3 IN ('stopped','failed') THEN $5 ELSE NULL END WHERE id=$1 AND node_id=$2 AND stopped_at_ms IS NULL AND NOT(status='pending' AND $3 NOT IN ('stopped','failed')) RETURNING id").bind(r.id).bind(node).bind(&r.status).bind(&r.stage).bind(at).fetch_optional(&mut *conn).await?;
        if changed.is_some() {
            owned.push(r);
        }
    }
    Ok(owned)
}

async fn commands(
    s: &Service,
    owned: Vec<&Report>,
    jobs: &[Session],
    at: i64,
) -> Result<BTreeMap<Uuid, Vec<Value>>> {
    let mut runtime = s.runtime.0.lock().await;
    for report in owned {
        let screen = terminal::screen(&mut runtime, report.id)?;
        report.apply(screen, at);
    }
    let commands = jobs
        .iter()
        .filter(|job| job.status == "running" && job.desired_running)
        .filter_map(|job| {
            let dispatched = runtime.get_mut(&job.id)?.dispatch(at);
            (!dispatched.is_empty()).then_some((job.id, dispatched))
        })
        .collect();
    // Closed-session screens expire; no terminal bytes enter SQL or logs.
    runtime.retain(|_, screen| at - screen.seen < 300000);
    Ok(commands)
}
impl terminal::Screen {
    fn dispatch(&mut self, at: i64) -> Vec<Value> {
        // A streaming viewer sends directly to its fenced PTY connection.
        // Legacy requests still use the ordered command/receipt path.
        let current = self
            .controller
            .filter(|(_, _, until)| *until > at)
            .map(|_| self.generation);
        let mut ordered: Vec<_> = self.commands.values_mut().collect();
        ordered.sort_by_key(|cmd| cmd.sequence);
        ordered
            .into_iter()
            .filter_map(|command| command.dispatch(current, at))
            .collect()
    }
}
impl terminal::Command {
    fn acknowledge(&mut self, at: i64) {
        if matches!(self.disposition.as_str(), "dispatched" | "unknown") {
            self.disposition = "applied".into();
            self.updated = at;
        }
    }
    fn dispatch(&mut self, generation: Option<u64>, at: i64) -> Option<Value> {
        if self.disposition != "queued" {
            return None;
        }
        if generation != Some(self.generation) {
            self.disposition = "rejected".into();
            return None;
        }
        self.disposition = "dispatched".into();
        self.updated = at;
        Some(json!(self))
    }
}
