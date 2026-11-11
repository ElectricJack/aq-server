//! Shared provider/class admission for queued offers and interactive agents.
//! A reservation is the existing offered attempt or preparing manual session;
//! its owner's failure/stop transaction releases it, without a second lease.
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const CEILING: &str = "swarm_concurrency_limit_reached";
pub const UNCLASSIFIED: &str = "swarm_intelligence_class_unavailable";
pub fn is_wait_reason(reason: &str) -> bool {
    matches!(reason, CEILING | UNCLASSIFIED)
}
pub fn is_wait(reasons: &[String]) -> bool {
    reasons.iter().any(|reason| is_wait_reason(reason))
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Cell {
    pub provider: String,
    pub intelligence_class: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Count {
    pub provider: String,
    pub intelligence_class: String,
    /// Includes offers and preparation reservations, not only started processes.
    pub running: u32,
    pub waiting: u32,
}

/// Prefix-migration fixtures predate controls and preserve their old behavior.
pub async fn available(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT to_regclass('swarm_settings') IS NOT NULL")
        .fetch_one(conn)
        .await
}
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}
/// Resolve the selected profile identity from its pinned catalog. Profile IDs
/// remain stable across revisions. The automatic router is its own class;
/// neither its model choice nor capacity changes the task's routing class.
pub fn classify(catalog: &Value, reference: Option<&str>, choice: &Value) -> Option<Cell> {
    let provider = text(choice, "provider")?;
    let profiles = catalog.get("catalog")?.get("intelligence")?.as_array()?;
    let profile = profiles.iter().find(|p| {
        reference.is_some_and(|r| r == format!("{}@{}", text(p, "id").unwrap_or(""), p["version"]))
    })?;
    let matches = profile["candidates"].as_array()?.iter().any(|c| {
        text(c, "provider") == Some(provider)
            && text(c, "model") == text(choice, "model")
            && choice
                .get("reasoning_configuration_reference")
                .is_none_or(|r| c.get("reasoning_configuration_reference") == Some(r))
            && choice
                .get("harness")
                .is_none_or(|h| c["harnesses"].as_array().is_some_and(|hs| hs.contains(h)))
    });
    matches.then(|| Cell {
        provider: provider.into(),
        intelligence_class: text(profile, "id").unwrap().into(),
    })
}
async fn catalog_bytes(
    conn: &mut PgConnection,
    digest: Option<&str>,
) -> Result<Option<Value>, sqlx::Error> {
    let bytes:Option<Vec<u8>> = match digest {
        Some(d) => sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1").bind(d).fetch_optional(&mut *conn).await?,
        None => sqlx::query_scalar("SELECT a.artifact FROM profile_catalog_registry r JOIN profile_catalog_artifacts a ON a.digest=r.active_digest WHERE r.singleton").fetch_optional(&mut *conn).await?,
    };
    Ok(bytes.and_then(|b| serde_json::from_slice(&b).ok()))
}
pub async fn resolve(
    conn: &mut PgConnection,
    digest: Option<&str>,
    reference: Option<&str>,
    choice: &Value,
) -> Result<Option<Cell>, sqlx::Error> {
    Ok(catalog_bytes(conn, digest)
        .await?
        .and_then(|c| classify(&c, reference, choice)))
}
/// Manual specs pin their resolved class/catalog when created. Historical specs
/// are resolved conservatively from their project catalog and saved selection.
pub async fn manual_cell(
    conn: &mut PgConnection,
    spec: &Value,
    project: Uuid,
) -> Result<Option<Cell>, sqlx::Error> {
    if let (Some(provider), Some(class)) =
        (text(spec, "provider"), text(spec, "intelligence_class"))
    {
        return Ok(Some(Cell {
            provider: provider.into(),
            intelligence_class: class.into(),
        }));
    }
    let digest: Option<String> = sqlx::query_scalar(
        "SELECT binding->>'catalog_digest' FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(project)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    let selected = &spec["intelligence_profile"];
    let reference =
        text(selected, "profile_id").map(|id| format!("{}@{}", id, selected["profile_version"]));
    resolve(conn, digest.as_deref(), reference.as_deref(), spec).await
}

async fn occupancy(conn: &mut PgConnection) -> Result<Vec<(Cell, bool)>, sqlx::Error> {
    let rows=sqlx::query("SELECT d.decision#>>'{inputs,project,catalog_digest}' AS catalog, d.decision#>>'{inputs,project,intelligence_profile}' AS profile, s.selection->'choice' AS choice FROM execution_attempts a LEFT JOIN intelligence_attempt_selections s USING(attempt_id) LEFT JOIN profiled_launches l USING(attempt_id) LEFT JOIN intelligence_decisions d USING(decision_id) WHERE a.kind='profiled_v1' AND a.holds_task AND NOT profiled_compute_stopped_valid(a.attempt_id,a.node_id) AND NOT EXISTS(SELECT 1 FROM lost_attempt_releases lost JOIN profiled_authority_hot h USING(attempt_id) WHERE lost.attempt_id=a.attempt_id AND lost.node_id=a.node_id AND h.state->>'status'='finished' AND h.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object('id','aq.lost-attempt-release/'||a.attempt_id::TEXT,'version','1','digest',lost.receipt_digest)))").fetch_all(&mut *conn).await?;
    let mut out = Vec::new();
    let mut catalogs = BTreeMap::new();
    for row in rows {
        let digest: Option<String> = row.get("catalog");
        let profile: Option<String> = row.get("profile");
        let choice: Value = row.get::<Option<Value>, _>("choice").unwrap_or(Value::Null);
        let key = digest.clone().unwrap_or_default();
        if !catalogs.contains_key(&key) {
            catalogs.insert(key.clone(), catalog_bytes(conn, digest.as_deref()).await?);
        }
        let cell = catalogs[&key]
            .as_ref()
            .and_then(|c| classify(c, profile.as_deref(), &choice));
        out.push((
            cell.unwrap_or_else(|| Cell {
                provider: text(&choice, "provider").unwrap_or("unknown").into(),
                intelligence_class: "__unclassified".into(),
            }),
            false,
        ));
    }
    let manual:Vec<(Uuid,Value,String,bool)>=sqlx::query_as("SELECT project_id,spec,status,desired_running FROM manual_agent_sessions WHERE stopped_at_ms IS NULL").fetch_all(&mut *conn).await?;
    for (project, spec, status, desired) in manual {
        if status == "pending" && !desired {
            continue;
        }
        let cell = manual_cell(conn, &spec, project)
            .await?
            .unwrap_or_else(|| Cell {
                provider: text(&spec, "provider").unwrap_or("unknown").into(),
                intelligence_class: "__unclassified".into(),
            });
        out.push((cell, status == "pending"));
    }
    // Central supervisor generations reserve before dispatch and retain the
    // pinned admission class even if the owner's supervisor profile changes.
    let supervisors:Vec<(Option<String>,Option<String>)>=sqlx::query_as("SELECT s.provider,s.intelligence_class FROM central_session_generations g LEFT JOIN swarm_supervisor_admissions s USING(actor_id,generation) WHERE g.state_bytes::jsonb#>>'{data,phase}' NOT IN ('stopped_reported','cancelled_before_start')").fetch_all(&mut *conn).await?;
    for (provider, class) in supervisors {
        out.push((
            Cell {
                provider: provider.unwrap_or_else(|| "unknown".into()),
                intelligence_class: class.unwrap_or_else(|| "__unclassified".into()),
            },
            false,
        ));
    }
    Ok(out)
}
pub async fn counts(conn: &mut PgConnection) -> Result<Vec<Count>, sqlx::Error> {
    let now: i64 =
        sqlx::query_scalar("SELECT (extract(epoch FROM clock_timestamp())*1000)::BIGINT")
            .fetch_one(&mut *conn)
            .await?;
    counts_at(conn, now).await
}
pub async fn counts_at(conn: &mut PgConnection, now: i64) -> Result<Vec<Count>, sqlx::Error> {
    if !available(conn).await? {
        return Ok(Vec::new());
    }
    let mut counts = BTreeMap::<Cell, (u32, u32)>::new();
    for (cell, waiting) in occupancy(conn).await? {
        let count = counts.entry(cell).or_default();
        if waiting {
            count.1 = count.1.saturating_add(1);
        } else {
            count.0 = count.0.saturating_add(1);
        }
    }
    let waits:Vec<(String,String,i64)>=sqlx::query_as("SELECT provider,intelligence_class,count(*) FROM swarm_waiting w WHERE expires_at_ms>$1 AND NOT EXISTS(SELECT 1 FROM execution_attempts a WHERE a.task_id=w.task_id AND a.holds_task) GROUP BY provider,intelligence_class").bind(now).fetch_all(&mut *conn).await?;
    for (provider, intelligence_class, n) in waits {
        let c = counts
            .entry(Cell {
                provider,
                intelligence_class,
            })
            .or_default();
        c.1 = c.1.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
    }
    Ok(counts
        .into_iter()
        .map(|(cell, (running, waiting))| Count {
            provider: cell.provider,
            intelligence_class: cell.intelligence_class,
            running,
            waiting,
        })
        .collect())
}
/// Caller retains this transaction until inserting its attempt or changing its
/// pending manual session to preparing. No external launch runs under the lock.
pub async fn check(
    conn: &mut PgConnection,
    cell: Option<&Cell>,
) -> Result<Option<&'static str>, sqlx::Error> {
    if !available(conn).await? {
        return Ok(None);
    }
    let settings = crate::lock(conn).await.map_err(|e| match e {
        crate::Error::Database(e) => e,
        _ => sqlx::Error::Protocol("invalid swarm settings".into()),
    })?;
    if settings.normal.is_none() {
        return Ok(None);
    }
    // A row version makes a stale SERIALIZABLE publisher retry after a manual
    // READ COMMITTED claimant wins the lock; SELECT FOR UPDATE alone does not.
    sqlx::query("UPDATE swarm_settings SET revision=revision WHERE singleton")
        .execute(&mut *conn)
        .await?;
    let Some(cell) = cell else {
        return Ok(Some(UNCLASSIFIED));
    };
    let Some(limit) = crate::effective_limit(&settings, &cell.provider, &cell.intelligence_class)
    else {
        return Ok(Some(UNCLASSIFIED));
    };
    let active = occupancy(conn).await?;
    if active.iter().any(|(c, w)| {
        !w && (c.provider == cell.provider || c.provider == "unknown")
            && c.intelligence_class == "__unclassified"
    }) {
        return Ok(Some(UNCLASSIFIED));
    }
    let n = active.iter().filter(|(c, w)| !w && c == cell).count();
    Ok((n >= limit as usize).then_some(CEILING))
}
/// Selection-only observation: routing has already chosen its requirement
/// class. Capacity refuses that choice and never asks the router to downgrade.
pub async fn observe(
    conn: &mut PgConnection,
    task: Uuid,
    cell: Option<&Cell>,
    now: i64,
) -> Result<Option<&'static str>, sqlx::Error> {
    let reason = check(conn, cell).await?;
    if !available(conn).await? {
        return Ok(reason);
    }
    // Observations are a short-lived projection, including tasks cancelled or
    // removed from the graph since their last poll. Reclaim expired rows on
    // writes; read-only settings snapshots never mutate or wait for cleanup.
    // The owning settings lock was acquired by check before these row locks.
    sqlx::query("WITH expired AS (SELECT task_id FROM swarm_waiting WHERE expires_at_ms<=$1 ORDER BY expires_at_ms,task_id LIMIT 128 FOR UPDATE SKIP LOCKED) DELETE FROM swarm_waiting w USING expired e WHERE w.task_id=e.task_id")
        .bind(now).execute(&mut *conn).await?;
    if let (Some(reason), Some(cell)) = (reason, cell) {
        sqlx::query("INSERT INTO swarm_waiting(task_id,provider,intelligence_class,reason,observed_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(task_id) DO UPDATE SET provider=EXCLUDED.provider,intelligence_class=EXCLUDED.intelligence_class,reason=EXCLUDED.reason,observed_at_ms=EXCLUDED.observed_at_ms,expires_at_ms=EXCLUDED.expires_at_ms").bind(task).bind(&cell.provider).bind(&cell.intelligence_class).bind(reason).bind(now).bind(now.saturating_add(60000)).execute(&mut *conn).await?;
    } else {
        clear_waiting(conn, task).await?;
    }
    Ok(reason)
}
pub async fn clear_waiting(conn: &mut PgConnection, task: Uuid) -> Result<(), sqlx::Error> {
    if available(conn).await? {
        sqlx::query("DELETE FROM swarm_waiting WHERE task_id=$1")
            .bind(task)
            .execute(conn)
            .await?;
    }
    Ok(())
}
