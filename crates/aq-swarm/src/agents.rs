//! Slot-holding attempts for the swarm read (spec 0007): every attempt that
//! holds a task on the page's nodes, one row per occupied slot. The read is one
//! statement for the attempts and one for their `ran_with` summaries, however
//! many attempts there are. Task titles live in the graph, so they are read
//! once per task and then served from a process cache.
use crate::class::intelligence_class;
use aq_dashboard_contract::{
    AgentPhase, EntityRef, ProjectRef, RanWith, Revision, SwarmAgent, TitledRef,
};
use sqlx::Row;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What one read asks for.
pub struct Query<'a> {
    pub nodes: &'a [Uuid],
    /// A project scope lists only that project's attempts.
    pub project: Option<Uuid>,
    /// The terminal runtime of this server; another runtime's terminal is not shown.
    pub runtime: Option<Uuid>,
    /// Whether the `attempt_ran_with` table exists (spec 0014).
    pub summaries: bool,
    pub now_ms: u64,
    pub limit: usize,
}

/// The attempts holding a slot, most recent first.
pub struct Held {
    /// `(node, row)`.
    pub agents: Vec<(Uuid, SwarmAgent)>,
    /// Rows in phase `running`.
    pub running: usize,
    /// More attempts hold a slot than `limit`.
    pub cut: bool,
}

const SQL: &str = "SELECT e.attempt_id, e.session_id, e.node_id, e.task_id, \
 a.s->>'status' AS status, a.s#>>'{stop,kind}' AS stop, \
 (COALESCE((a.s->>'revoked')::boolean,false) OR n.revoked_at IS NOT NULL) AS revoked, \
 (a.s->>'cancellation_id' IS NOT NULL OR COALESCE((a.s->>'stopped')::boolean,false)) AS ended, \
 COALESCE((a.s#>>'{grant,expires_at_ms}')::bigint,(a.s->>'grant_expires_at_ms')::bigint,0) AS grant_expires, \
 CASE WHEN h.attempt_id IS NULL THEN e.execution_deadline_ms ELSE (h.state->>'execution_deadline_ms')::bigint END AS deadline, \
 e.state#>>'{offer,manifest,model,provider}' AS provider, e.state#>>'{offer,manifest,model,model}' AS model, \
 e.state#>>'{offer,manifest,harness,harness}' AS harness, e.state#>>'{offer,manifest,bindings,task_revision}' AS task_revision, \
 p.id AS project_id, b.binding->>'name' AS project_name, \
 (convert_from(s.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint AS started_ms, \
 COALESCE((l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::bigint,(l.graph_task_observation->>'observed_at_ms')::bigint) AS held_ms, \
 t.terminal_id, t.terminal_epoch, t.runtime_id, t.last_output_at_ms \
 FROM execution_attempts e JOIN nodes n ON n.id=e.node_id \
 LEFT JOIN profiled_authority_hot h ON h.attempt_id=e.attempt_id \
 LEFT JOIN profiled_launches l ON l.attempt_id=e.attempt_id \
 LEFT JOIN profiled_started_receipts s ON s.attempt_id=e.attempt_id \
 LEFT JOIN terminal_sessions t ON t.attempt_id=e.attempt_id AND t.session_id=e.session_id \
 CROSS JOIN LATERAL (SELECT COALESCE(h.state,e.state) AS s) a \
 CROSS JOIN LATERAL (SELECT COALESCE(e.state#>>'{offer,manifest,bindings,project_id}',e.state#>>'{offer,spec,project_id}') AS id) p \
 LEFT JOIN project_execution_bindings b ON b.project_id::text=p.id \
 WHERE e.holds_task AND e.node_id = ANY($1) AND ($2::text IS NULL OR p.id=$2) \
 ORDER BY e.execution_deadline_ms DESC NULLS FIRST, e.node_id, e.session_id LIMIT $3";

fn ms(row: &sqlx::postgres::PgRow, column: &str) -> Option<u64> {
    let value = row.get::<Option<i64>, _>(column)?;
    u64::try_from(value).ok().filter(|v| *v > 0)
}

/// `running` is exactly what the overview counts (`AuthorityView::running_at`):
/// granted, started, not ended and inside its grant and execution deadline.
fn phase(row: &sqlx::postgres::PgRow, now_ms: u64) -> AgentPhase {
    let status: Option<String> = row.get("status");
    let stop: Option<String> = row.get("stop");
    let live = !row.get::<bool, _>("revoked") && !row.get::<bool, _>("ended");
    let grant = u64::try_from(row.get::<i64, _>("grant_expires")).unwrap_or(0);
    let open = now_ms > 0 && now_ms < grant && ms(row, "deadline").is_none_or(|d| now_ms < d);
    match (live, status.as_deref(), stop.as_deref()) {
        (true, Some("granted"), Some("running")) if open => AgentPhase::Running,
        (true, Some("offered" | "preparing" | "granted"), None | Some("unknown")) => {
            AgentPhase::Starting
        }
        _ => AgentPhase::Finishing,
    }
}

/// Every attempt holding a slot on `nodes`: two statements for the whole page.
pub async fn load(conn: &mut sqlx::PgConnection, q: Query<'_>) -> sqlx::Result<Held> {
    let time = |v: Option<u64>| {
        v.map(crate::timestamp)
            .transpose()
            .map_err(|_| sqlx::Error::Protocol("time".into()))
    };
    let modern = aq_machine_control::slots::available(conn)
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let query = if modern {
        SQL.replace("SELECT e.attempt_id,", "SELECT x.slot, e.attempt_id,").replace("LEFT JOIN profiled_authority_hot h", "LEFT JOIN execution_attempt_slots x ON x.attempt_id=e.attempt_id LEFT JOIN profiled_authority_hot h")
    } else {
        SQL.replace(
            "SELECT e.attempt_id,",
            "SELECT NULL::INTEGER AS slot, e.attempt_id,",
        )
    };
    let mut rows = sqlx::query(&query)
        .bind(q.nodes)
        .bind(q.project.map(|p| p.to_string()))
        .bind(i64::try_from(q.limit).unwrap_or(i64::MAX).saturating_add(1))
        .fetch_all(&mut *conn)
        .await?;
    let cut = rows.len() > q.limit;
    rows.truncate(q.limit);
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get("attempt_id")).collect();
    let mut summaries = match q.summaries && !ids.is_empty() {
        true => aq_role_settings::attempts::ran_with_many(conn, &ids)
            .await
            .map_err(|_| sqlx::Error::Protocol("ran_with".into()))?,
        false => BTreeMap::new(),
    };
    let mut held = Held {
        agents: Vec::with_capacity(rows.len()),
        running: 0,
        cut,
    };
    for row in &rows {
        let attempt: Uuid = row.get("attempt_id");
        let task: Uuid = row.get("task_id");
        let phase = phase(row, q.now_ms);
        held.running += usize::from(phase == AgentPhase::Running);
        let project = (row.get::<Option<String>, _>("project_id"))
            .and_then(|p| p.parse::<Uuid>().ok())
            .unwrap_or_default();
        let name = row.get::<Option<String>, _>("project_name");
        let model: Option<String> = row.get("model");
        let revision = row.get::<Option<String>, _>("task_revision").filter(|r| {
            r.parse::<u128>()
                .is_ok_and(|n| n > 0 && n.to_string() == *r)
        });
        // A terminal is shown only when this server's runtime registered it.
        let shown = !row.get::<bool, _>("revoked")
            && q.runtime.is_some()
            && row.get::<Option<Uuid>, _>("runtime_id") == q.runtime;
        let terminal = row.get::<Option<Uuid>, _>("terminal_id").filter(|_| shown);
        let held_since = ms(row, "held_ms");
        let started = ms(row, "started_ms").filter(|_| phase != AgentPhase::Starting);
        let agent = SwarmAgent {
            slot: row.get::<Option<i32>, _>("slot").map(|v| v as u32),
            session_id: row.get("session_id"),
            attempt_id: attempt,
            task: Some(TitledRef {
                kind: "task".into(),
                id: task,
                revision: revision.map(|r| Revision(r).into()),
                title: task.to_string(),
            }),
            project: ProjectRef {
                id: project,
                label: name
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .unwrap_or_else(|| project.to_string()),
            },
            provider: row.get("provider"),
            intelligence_class: model.as_deref().map(intelligence_class),
            model,
            harness: row.get("harness"),
            routing_profile: None,
            terminal: terminal.map(|id| EntityRef {
                kind: "terminal".into(),
                id,
                revision: (row.get::<Option<i64>, _>("terminal_epoch"))
                    .map(|e| Revision(e.to_string()).into()),
            }),
            started_at: time(started.or(held_since))?,
            phase: Some(phase),
            held_since: time(held_since)?,
            ran_with: summaries.remove(&attempt).map(|r| RanWith {
                role: r.role,
                level: r.level,
                provider: r.provider,
                model: r.model,
                reasoning: r.reasoning,
                harness: r.harness,
                execution_profile: r.execution_profile,
                snapshot_digest: r.snapshot_digest,
            }),
            last_output_at: time(ms(row, "last_output_at_ms").filter(|_| shown))?,
        };
        held.agents.push((row.get("node_id"), agent));
    }
    Ok(held)
}

/// How long a title read from the graph is served, and how long a task whose
/// title could not be read is left alone.
const TITLE_TTL: Duration = Duration::from_secs(300);
const NO_TITLE_TTL: Duration = Duration::from_secs(30);
/// Time one read spends on graph titles it has not cached yet.
pub const TITLE_BUDGET: Duration = Duration::from_millis(1500);
const MAX_TITLES: usize = 4096;
const MAX_TITLE_CHARS: usize = 256;
static TITLES: Mutex<BTreeMap<Uuid, (Option<String>, Instant)>> = Mutex::new(BTreeMap::new());

/// The cached title of `task`: `None` when it must be read, `Some(None)` when
/// a recent read found none.
pub fn title(task: Uuid) -> Option<Option<String>> {
    let titles = TITLES.lock().unwrap_or_else(|e| e.into_inner());
    let (title, at) = titles.get(&task)?;
    let ttl = if title.is_some() {
        TITLE_TTL
    } else {
        NO_TITLE_TTL
    };
    (at.elapsed() < ttl).then(|| title.clone())
}

/// The cached title of `task`, or its id while no title has been read.
pub fn title_or_id(task: Uuid) -> String {
    (title(task).flatten()).unwrap_or_else(|| task.to_string())
}

pub fn remember(task: Uuid, title: Option<String>) {
    let title = title
        .map(|t| t.trim().chars().take(MAX_TITLE_CHARS).collect::<String>())
        .filter(|t| !t.is_empty());
    let mut titles = TITLES.lock().unwrap_or_else(|e| e.into_inner());
    if titles.len() >= MAX_TITLES {
        titles.clear();
    }
    titles.insert(task, (title, Instant::now()));
}

impl Held {
    /// Gives each row its task's title. `read(project, task)` is called once
    /// for each task not in the cache; `None` from it stops further reads for
    /// this page (the rows left keep the task id as their title).
    pub async fn titles<F, Fut>(&mut self, mut read: F)
    where
        F: FnMut(Uuid, Uuid) -> Fut,
        Fut: Future<Output = Option<Option<String>>>,
    {
        let mut reading = true;
        for (_, agent) in &mut self.agents {
            let project = agent.project.id;
            let Some(task) = agent.task.as_mut() else {
                continue;
            };
            let mut known = title(task.id);
            if known.is_none() && reading {
                match read(project, task.id).await {
                    Some(found) => {
                        remember(task.id, found);
                        known = title(task.id);
                    }
                    None => reading = false,
                }
            }
            if let Some(Some(found)) = known {
                task.title = found;
            }
        }
    }
}
