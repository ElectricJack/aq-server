use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
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
    if !(1..=8).contains(&v.max_slots)
        || v.repositories.len() > 16
        || v.reports.len() > 32
        || v.providers.len() > 2
        || v.providers
            .iter()
            .any(|p| !["anthropic", "openai"].contains(&p.as_str()))
    {
        return Err(bad("Invalid manual worker bounds"));
    }
    for r in &v.reports {
        if !["preparing", "running", "stopping", "stopped", "failed"].contains(&r.status.as_str())
            || r.stage.len() > 128
            || r.snapshot.as_ref().is_some_and(|s| s.len() > 131072)
            || r.acknowledged.len() > 128
        {
            return Err(bad("Invalid session report"));
        }
    }
    let at = now();
    let mut tx = s.pool.begin().await?;
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
    if desired.as_deref() == Some("stopped") {
        sqlx::query("UPDATE manual_agent_sessions SET desired_running=false WHERE node_id=$1 AND stopped_at_ms IS NULL").bind(node).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO manual_worker_reports(node_id,max_slots,repositories,observed_at_ms,providers) VALUES($1,$2,$3,$4,$5) ON CONFLICT(node_id) DO UPDATE SET max_slots=EXCLUDED.max_slots,repositories=EXCLUDED.repositories,observed_at_ms=EXCLUDED.observed_at_ms,providers=EXCLUDED.providers").bind(node).bind(v.max_slots as i32).bind(json!(v.repositories)).bind(at).bind(json!(v.providers)).execute(&mut *tx).await?;
    let mut owned = Vec::new();
    for r in &v.reports {
        let changed=sqlx::query("UPDATE manual_agent_sessions SET status=$3,stage=$4,updated_at_ms=CASE WHEN status<>$3 OR stage<>$4 THEN $5 ELSE updated_at_ms END,worker_seen_at_ms=$5,stopped_at_ms=CASE WHEN $3 IN ('stopped','failed') THEN $5 ELSE NULL END WHERE id=$1 AND node_id=$2 AND stopped_at_ms IS NULL AND NOT(status='pending' AND $3='running') RETURNING id").bind(r.id).bind(node).bind(&r.status).bind(&r.stage).bind(at).fetch_optional(&mut *tx).await?;
        if changed.is_some() {
            owned.push(r);
        }
    }
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
    let mut used = BTreeMap::<Uuid, u32>::new();
    let mut total = 0;
    for x in &sessions {
        if x.status != "pending" {
            total += 1;
            *used.entry(x.project_id).or_default() += 1;
        }
    }
    let mut jobs = Vec::new();
    for x in &mut sessions {
        if x.status == "pending" && x.desired_running {
            let limit = slots
                .iter()
                .find(|(id, _)| *id == x.project_id)
                .map_or(1, |(_, v)| *v as u32);
            if desired.as_deref().is_some_and(|v| v != "running")
                || v.busy
                || total >= v.max_slots
                || used.get(&x.project_id).copied().unwrap_or(0) >= limit
            {
                continue;
            }
            total += 1;
            *used.entry(x.project_id).or_default() += 1;
            x.status = "preparing".into();
            x.stage = "Preparing workspace".into();
            sqlx::query("UPDATE manual_agent_sessions SET status='preparing',stage='Preparing workspace',updated_at_ms=$2 WHERE id=$1 AND status='pending'").bind(x.id).bind(at).execute(&mut *tx).await?;
        }
        jobs.push(x.clone());
    }
    tx.commit().await?;
    let mut runtime = s.runtime.0.lock().await;
    for report in owned {
        let screen = terminal::screen(&mut runtime, report.id)?;
        screen.seen = at;
        if let Some(text) = &report.snapshot
            && (screen.text != *text || screen.sequence == 0)
        {
            screen.text = text.clone();
            screen.sequence += 1;
        }
        for id in &report.acknowledged {
            if let Some(c) = screen.commands.get_mut(id)
                && matches!(c.disposition.as_str(), "dispatched" | "unknown")
            {
                c.disposition = "applied".into();
                c.updated = at;
            }
        }
    }
    let mut commands = BTreeMap::<Uuid, Vec<Value>>::new();
    for job in &jobs {
        if job.status != "running" || !job.desired_running {
            continue;
        }
        if let Some(screen) = runtime.get_mut(&job.id) {
            // A streaming viewer sends directly to its fenced PTY connection.
            // Legacy requests still use the ordered command/receipt path.
            let current = screen
                .controller
                .filter(|(_, _, until)| *until > at)
                .map(|_| screen.generation);
            let mut ordered: Vec<_> = screen.commands.values_mut().collect();
            ordered.sort_by_key(|cmd| cmd.sequence);
            for cmd in ordered {
                if cmd.disposition == "queued" {
                    if current != Some(cmd.generation) {
                        cmd.disposition = "rejected".into();
                        continue;
                    }
                    cmd.disposition = "dispatched".into();
                    cmd.updated = at;
                    commands.entry(job.id).or_default().push(json!(cmd));
                }
            }
        }
    }
    // Closed-session screens expire; no terminal bytes enter SQL or logs.
    runtime.retain(|_, screen| at - screen.seen < 300000);
    Ok(Json(
        json!({"sessions":jobs,"commands":commands,"pending_count":sessions.iter().filter(|s|s.desired_running).count(),"lease_ms":45000,"server_time_ms":at}),
    ))
}
