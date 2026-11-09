//! Read-only, exact-session views. These snapshots never grant control or expose
//! another attempt's usage through a session inspection ACL.
use super::*;
use crate::{error::AppError, session_access};
use aq_lifecycle_api::{AccountingCompleteness, UsageView};

pub use aq_observation_api::{AccountingMark, EventView, EventsPage, SessionUsage};
fn reason_code<T: Serialize>(reason: T) -> std::result::Result<Text, AppError> {
    let v = serde_json::to_value(reason).map_err(|_| invalid())?;
    Text::new(v.as_str().ok_or_else(invalid)?).map_err(|_| invalid())
}
fn invalid() -> AppError {
    AppError::Internal
}
fn json<T: serde::de::DeserializeOwned>(value: Value) -> std::result::Result<T, AppError> {
    serde_json::from_value(value).map_err(|_| invalid())
}
async fn authorize(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
    attempt: Uuid,
) -> std::result::Result<(Binding, session_access::InspectionAccess), AppError> {
    if attempt.is_nil() {
        return Err(AppError::BadRequest("invalid attempt".into()));
    }
    let node: Option<Uuid> =
        sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| invalid())?;
    let node = node.ok_or_else(|| AppError::NotFound("session not found".into()))?;
    let historical = AuthorityPolicy::TerminalHistorical;
    let view = authority::read_attempt(state, tx, node, attempt, historical).await?;
    if view.kind != Kind::ProfiledV1 {
        return Err(AppError::NotFound("profiled session not found".into()));
    }
    let access = session_access::require_inspect_locked(state, tx, &view, actor).await?;
    let graph = view.graph.as_ref().ok_or_else(invalid)?;
    let binding = Binding {
        project_id: view.project_id,
        task_id: view.task_id,
        lineage_id: aq_admission::transport::task_lineage_id(
            view.project_id,
            graph.workspace_id,
            view.task_id,
        )
        .map_err(|_| invalid())?,
        attempt_id: view.attempt.attempt_id,
        attempt_generation: view.attempt.generation,
        session: view.session,
    };
    binding.validate().map_err(|_| invalid())?;
    Ok((binding, access))
}
fn final_time(
    state: &AppState,
    access: &session_access::InspectionAccess,
) -> std::result::Result<u64, AppError> {
    let now = (state.clock)().timestamp_millis().max(0) as u64;
    if now == 0 || now < access.checked_at_ms || now >= access.valid_until_ms {
        return Err(AppError::Forbidden("session inspection expired".into()));
    }
    Ok(now)
}
pub async fn events(
    state: &AppState,
    actor: Uuid,
    attempt: Uuid,
    after: u64,
    through: Option<u64>,
    limit: u32,
) -> std::result::Result<EventsPage, AppError> {
    if !(1..=16).contains(&limit)
        || after > i64::MAX as u64
        || through.is_some_and(|v| v > i64::MAX as u64 || v < after)
    {
        return Err(AppError::BadRequest("invalid observation page".into()));
    }
    let snapshot = authority::snapshot(&state.store.observation_pool).await;
    let mut tx = snapshot.map_err(crate::error::db)?;
    let (binding, access) = authorize(state, &mut tx, actor, attempt).await?;
    let node = node_of(&binding).map_err(|_| invalid())?;
    let latest: Option<i64> =
        sqlx::query_scalar("SELECT through_sequence FROM observation_nodes WHERE node_id=$1")
            .bind(node)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| invalid())?;
    let latest = latest.unwrap_or(0) as u64;
    let through = through.unwrap_or(latest);
    if through > latest || after > through {
        return Err(AppError::Conflict(
            "observation cursor is beyond retained admission".into(),
        ));
    }
    let rows=sqlx::query("SELECT sequence,record,decision,recorded_at_ms FROM observation_events WHERE attempt_id=$1 AND node_id=$2 AND sequence>$3 AND sequence<=$4 ORDER BY sequence LIMIT $5").bind(attempt).bind(node).bind(after as i64).bind(through as i64).bind(limit as i64+1).fetch_all(&mut *tx).await.map_err(|_|invalid())?;
    let mut page = EventsPage {
        schema_version: 1,
        binding: binding.clone(),
        after_sequence: after,
        through_sequence: through,
        events: vec![],
        next_after: None,
        complete_for_snapshot: true,
        checked_at_ms: access.checked_at_ms,
    };
    let mut bytes = serde_json::to_vec(&page).map_err(|_| invalid())?.len() + 256;
    for row in rows {
        if page.events.len() >= limit as usize {
            page.complete_for_snapshot = false;
            break;
        }
        let record: ObservationRecord = json(row.get("record"))?;
        record.validate().map_err(|_| invalid())?;
        if record.body.binding != binding
            || integer(record.body.sequence).map_err(|_| invalid())?
                != row.get::<i64, _>("sequence")
        {
            return Err(invalid());
        }
        let decision: Value = row.get("decision");
        let accounting = if record.body.kind.is_usage() {
            match json::<Decision>(decision)? {
                Decision::Append {
                    fact, supersedes, ..
                } => {
                    fact.validate().map_err(|_| invalid())?;
                    if fact.event.event_id != record.event_id
                        || fact.event.digest != record.body_digest
                        || fact.lineage != binding.lineage_id
                    {
                        return Err(invalid());
                    }
                    AccountingMark::ContributionRecorded {
                        event: fact.event,
                        supersedes,
                    }
                }
                Decision::NoCharge { reason } => AccountingMark::NotCharged {
                    reason: reason_code(reason)?,
                },
                Decision::Unresolved { reason } => AccountingMark::Unresolved {
                    reason: reason_code(reason)?,
                },
            }
        } else {
            if decision != json!({"kind":"observation_only"}) {
                return Err(invalid());
            }
            AccountingMark::ObservationOnly
        };
        let received: i64 = row.get("recorded_at_ms");
        if received <= 0 {
            return Err(invalid());
        }
        let item = EventView {
            record,
            recorded_at_ms: received as u64,
            accounting,
        };
        let size = serde_json::to_vec(&item).map_err(|_| invalid())?.len() + 1;
        if bytes + size > 262144 {
            page.complete_for_snapshot = false;
            break;
        }
        bytes += size;
        page.events.push(item);
    }
    if !page.complete_for_snapshot {
        page.next_after = Some(page.events.last().ok_or_else(invalid)?.record.body.sequence);
    }
    page.checked_at_ms = final_time(state, &access)?;
    if serde_json::to_vec(&page).map_err(|_| invalid())?.len() > 262144 {
        return Err(invalid());
    }
    page.validate().map_err(|_| invalid())?;
    tx.commit().await.map_err(|_| invalid())?;
    Ok(page)
}
pub async fn usage(
    state: &AppState,
    actor: Uuid,
    attempt: Uuid,
) -> std::result::Result<SessionUsage, AppError> {
    let snapshot = authority::snapshot(&state.store.observation_pool).await;
    let mut tx = snapshot.map_err(crate::error::db)?;
    let (binding, access) = authorize(state, &mut tx, actor, attempt).await?;
    let row=sqlx::query("SELECT revision,last_recorded_at_ms,projection FROM observation_attempt_projections WHERE attempt_id=$1").bind(attempt).fetch_optional(&mut *tx).await.map_err(|_|invalid())?;
    let (revision, last, projection) = if let Some(row) = row {
        (
            row.get::<i64, _>("revision"),
            row.get::<i64, _>("last_recorded_at_ms"),
            json::<Projection>(row.get("projection"))?,
        )
    } else {
        (0, 0, Projection::default())
    };
    if revision < 0 || last < 0 {
        return Err(invalid());
    }
    projection.validate().map_err(|_| invalid())?;
    let reference = ArtifactRef {
        id: Text::new("aq.session-usage").map_err(|_| invalid())?,
        version: Text::new(revision.to_string()).map_err(|_| invalid())?,
        digest: Digest::of(
            &serde_json::to_vec(&(&binding, revision, &projection)).map_err(|_| invalid())?,
        ),
    };
    let facts = budget::priced_facts(&mut tx, budget::Scope::Attempt(attempt))
        .await
        .map_err(|_| invalid())?;
    let summary = budget::summarize(state, &facts).map_err(|_| invalid())?;
    let mut costs = summary.costs.clone();
    if costs.is_empty() {
        costs.push(Cost::Unknown {
            reason: UnknownReason::NotReported,
        });
    }
    let completeness = if projection.canonical_calls == 0 && projection.unresolved_events == 0 {
        AccountingCompleteness::Unknown
    } else if summary.complete
        && projection.incomplete_calls == 0
        && projection.unresolved_events == 0
        && facts.len() as u64 == projection.canonical_calls
    {
        AccountingCompleteness::CompleteForPolicy {
            policy: ArtifactRef {
                id: Text::new("aq.accounting.cost-policy").map_err(|_| invalid())?,
                version: Text::new("1").map_err(|_| invalid())?,
                digest: Digest::of(&serde_json::to_vec(&costs).map_err(|_| invalid())?),
            },
        }
    } else {
        let reason = summary
            .reason
            .unwrap_or_else(|| "native stream coverage or cost evidence is incomplete".to_owned());
        AccountingCompleteness::Incomplete {
            reason: Text::new(reason).map_err(|_| invalid())?,
        }
    };
    let usage = UsageView {
        ledger_revision: reference,
        counters: projection.counters().map_err(|_| invalid())?,
        counter_generation: None,
        costs,
        completeness,
    };
    usage.validate().map_err(|_| invalid())?;
    let snapshot = SessionUsage {
        schema_version: 1,
        binding,
        revision: revision as u64,
        checked_at_ms: final_time(state, &access)?,
        last_recorded_at_ms: (last > 0).then_some(last as u64),
        usage,
        known_category_subtotals: projection.known_subtotals,
        canonical_calls: projection.canonical_calls,
        incomplete_calls: projection.incomplete_calls,
        unresolved_events: projection.unresolved_events,
    };
    snapshot.validate().map_err(|_| invalid())?;
    tx.commit().await.map_err(|_| invalid())?;
    Ok(snapshot)
}
