//! Host catalog 2.3.0 (Epic 0, spec 0016 section 6.2), the decision commands
//! `work.complete`, `work.return`, `work.hold` and `supervisor.notify`: which
//! task and result a command names, what it answers, and the rows it writes
//! (`delivery_decisions`, `delivery_tasks.returns`,
//! `delivery_notifications`). The server's intent rows, settlement and
//! supervisor message are in aq-server. Compiled in only with the cargo
//! feature `delivery_playbook`.
//!
//! Mechanism only: nothing here judges a result. A decision is per result: a
//! result with one is not announced again. `held` waits for a later decision;
//! `completed` and `returned` are final. `returns` counts each result sent
//! back once.
use crate::{
    Error, Result,
    delivery_store::{DeliveryPort, locate},
    digest,
    host_values::{HostResult, text},
    host10::ResultReady,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// What the server's settlement did to a result for `work.complete` or
/// `work.return`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Settled {
    /// A validated Graph receipt completed the task and released the claim.
    Completed,
    /// The result is refused for rework: the task goes to a new attempt.
    Returned,
    /// Nothing changed: the attempt moved past the result (it is not the
    /// current report head, the attempt no longer holds the task, its stop is
    /// not proven, it was revoked or cancelled), or the result was settled
    /// the other way.
    Stale,
    /// Dispatched with no Graph answer yet: a replay sends the same payload.
    Pending,
}

/// The server's settlement, as `work.complete` and `work.return` call it. A
/// port, so that the commands can be tested without a recorded result.
#[async_trait::async_trait]
pub trait DeliverySettlement: Send + Sync {
    /// Completes the task with `result`.
    async fn complete(&self, result: Uuid) -> Result<Settled>;
    /// Refuses `result` for rework with the playbook's `reason` code.
    async fn send_back(&self, result: Uuid, reason: &str) -> Result<Settled>;
}

/// The task and result a decision command names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    /// The task's latest result.
    Latest { task: Uuid, result: Uuid },
    /// A task of a served project that has no result: `unknown_result` if
    /// the task exists, `unknown_task` otherwise (the caller reads the
    /// Graph, in the namespace's workspace).
    Resultless(Uuid),
    /// The catalog's refusal reason: `unknown_task` (no such task, or one
    /// of a project this namespace does not deliver for), `unknown_result`
    /// or `stale_result` (a result of the task, but not its latest).
    Refused(&'static str),
}

/// What `input` (`task`, `result_id`) names. `own` is the namespace's
/// workspace and project; as for `aq.work.result_ready`, the project served
/// is the namespace's, or every bound project with `installation_routing`.
pub async fn named(
    port: &dyn DeliveryPort,
    input: &Value,
    own: (Uuid, Uuid),
    installation_routing: bool,
) -> Result<Named> {
    let Ok(task) = Uuid::parse_str(text(input, "task")?) else {
        return Ok(Named::Refused("unknown_task"));
    };
    let (latest, at, bound) = locate(port, task, own).await?;
    if !bound || (at.1 != own.1 && !installation_routing) {
        return Ok(Named::Refused("unknown_task"));
    }
    let Some(latest) = latest else {
        return Ok(Named::Resultless(task));
    };
    let Ok(result) = Uuid::parse_str(text(input, "result_id")?) else {
        return Ok(Named::Refused("unknown_result"));
    };
    if latest.result_id == result {
        return Ok(Named::Latest { task, result });
    }
    Ok(Named::Refused(
        match port.has_result(task, result).await? {
            true => "stale_result",
            false => "unknown_result",
        },
    ))
}

fn answer(outcome: &str) -> HostResult {
    HostResult {
        outcome: outcome.into(),
        output: json!({}),
    }
}
pub fn refused(reason: &str) -> HostResult {
    HostResult {
        outcome: "refused".into(),
        output: json!({"reason": reason}),
    }
}

/// The answer of `work.complete` (`completed`) or `work.return`
/// (`returned`) on a result whose `decision` is already recorded: its own
/// outcome again (a later run's call settles nothing more, and `returns` is
/// not counted again), `stale_result` for the other final decision, and
/// `None` when the settlement is still to run (no decision, or a hold).
pub fn already(command: &str, decision: Option<&str>) -> Option<HostResult> {
    let own = outcome_of(command);
    match decision {
        Some(d @ ("completed" | "returned")) if d == own => Some(answer(own)),
        Some("completed" | "returned") => Some(refused("stale_result")),
        _ => None,
    }
}

/// The answer to `work.complete` or `work.return` once the settlement ran,
/// with the decision to record; `None` while the settlement is pending.
pub fn settled(command: &str, settled: Settled) -> Option<(HostResult, Option<&'static str>)> {
    let own = outcome_of(command);
    match settled {
        Settled::Completed | Settled::Returned if decision_of(settled) == own => {
            Some((answer(own), Some(own)))
        }
        Settled::Stale => Some((refused("stale_result"), None)),
        _ => None,
    }
}
fn outcome_of(command: &str) -> &'static str {
    match command {
        "work.complete" => "completed",
        _ => "returned",
    }
}
fn decision_of(settled: Settled) -> &'static str {
    match settled {
        Settled::Completed => "completed",
        _ => "returned",
    }
}
/// The `held` answer of `work.hold`.
pub fn held() -> HostResult {
    answer("held")
}
/// The `sent` or `duplicate` answer of `supervisor.notify`.
pub fn notified(duplicate: bool) -> HostResult {
    answer(if duplicate { "duplicate" } else { "sent" })
}

/// The decision recorded on `result`, if any.
pub async fn decision(db: &mut PgConnection, result: Uuid) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT decision FROM delivery_decisions WHERE result_id=$1")
            .bind(result)
            .fetch_optional(db)
            .await?,
    )
}

/// A decision on one result. `detail` and `update_from_main` are given only
/// for `returned`.
#[derive(Clone, Copy, Debug)]
pub struct Decision<'a> {
    pub task: Uuid,
    pub result: Uuid,
    pub decision: &'a str,
    pub reason: Option<&'a str>,
    pub detail: Option<&'a str>,
    pub update_from_main: Option<bool>,
}

/// Records `d`: a first decision on the result, or one that ends a hold. A
/// final decision is never replaced, and a hold never replaces a decision. A
/// result sent back for the first time adds one to the task's `returns`.
/// Returns whether it recorded anything.
pub async fn record(db: &mut PgConnection, d: &Decision<'_>, now_ms: i64) -> Result<bool> {
    let recorded = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO delivery_decisions(result_id,task_id,decision,reason,detail,update_from_main,decided_at_ms) \
         VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(result_id) DO UPDATE SET decision=EXCLUDED.decision,\
         reason=EXCLUDED.reason,detail=EXCLUDED.detail,update_from_main=EXCLUDED.update_from_main,\
         decided_at_ms=EXCLUDED.decided_at_ms \
         WHERE delivery_decisions.decision='held' AND EXCLUDED.decision<>'held' RETURNING result_id",
    )
    .bind(d.result)
    .bind(d.task)
    .bind(d.decision)
    .bind(d.reason)
    .bind(d.detail)
    .bind(d.update_from_main)
    .bind(now_ms)
    .fetch_optional(&mut *db)
    .await?
    .is_some();
    if recorded && d.decision == "returned" {
        sqlx::query("INSERT INTO delivery_tasks(task_id,returns) VALUES($1,1) ON CONFLICT(task_id) DO UPDATE SET returns=LEAST(delivery_tasks.returns+1,1000)")
            .bind(d.task)
            .execute(&mut *db)
            .await?;
    }
    Ok(recorded)
}

/// Fence a hold before looking up its named result or writing its receipt.
pub async fn fence_hold(
    db: &mut PgConnection,
    port: &dyn DeliveryPort,
    origin: Option<ResultReady>,
    input: &Value,
) -> Result<()> {
    if let Some(origin) = origin
        && (!origin.names_result(input) || !port.lock_current(db, &origin).await?)
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

/// Records a hold while keeping a stale run from receiving a held receipt.
pub async fn hold(
    db: &mut PgConnection,
    named: std::result::Result<(Uuid, Uuid), &str>,
    input: &Value,
    now: i64,
) -> Result<()> {
    let (task, result) = match named {
        Ok(named) => named,
        Err("stale_result") => return Err(Error::Conflict),
        Err(_) => return Err(Error::Invalid),
    };
    if matches!(
        decision(db, result).await?.as_deref(),
        Some("completed" | "returned")
    ) {
        return Err(Error::Conflict);
    }
    record(
        db,
        &Decision {
            task,
            result,
            decision: "held",
            reason: Some(text(input, "reason")?),
            detail: None,
            update_from_main: None,
        },
        now,
    )
    .await?;
    Ok(())
}

/// The notification for a delivery event is tied to a recorded hold on its current result.
pub async fn notification(
    db: &mut PgConnection,
    port: &dyn DeliveryPort,
    origin: Option<ResultReady>,
    project: Uuid,
    input: &Value,
) -> Result<(String, String)> {
    let body = text(input, "text")?;
    let Some(origin) = origin else {
        return Ok((notify_key(text(input, "identity")?)?, body.to_owned()));
    };
    if origin.project_id != project || !port.lock_current(db, &origin).await? {
        return Err(Error::Conflict);
    }
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM delivery_decisions WHERE result_id=$1 AND task_id=$2 AND decision='held'",
    )
    .bind(origin.result_id)
    .bind(origin.task_id)
    .fetch_optional(&mut *db)
    .await?
    .flatten();
    let reason = reason.ok_or(Error::Conflict)?;
    let key = notify_key(&format!("{}:{reason}", origin.result_id))?;
    let context = format!(
        "Delivery hold {reason} for project {}, task {} (revision {}), attempt {}, result {}.",
        origin.project_id, origin.task_id, origin.version, origin.attempt_id, origin.result_id
    );
    let latest = port
        .latest_in(db, origin.task_id)
        .await?
        .filter(|r| {
            r.project_id == project
                && r.attempt_id == origin.attempt_id
                && r.result_id == origin.result_id
        })
        .ok_or(Error::Conflict)?;
    let head = if latest.head.is_empty() {
        "none recorded"
    } else {
        &latest.head
    };
    let mut evidence = format!(
        "Recorded outcome: {}. Changes from the lineage base: {}. Retained head: {head}.",
        latest.outcome, latest.changed
    );
    let mut check_log = None;
    if reason == "returns_exhausted"
        && let Some(pr) = crate::delivery_store::held_pr_evidence(db, origin.result_id).await?
    {
        check_log = crate::delivery_store::held_check_log(db, origin.result_id).await?;
        evidence.push(' ');
        evidence.push_str(&pr);
    }
    let notice = format!(
        "{context}\n{evidence}\n\n{body}\n\nUse task.work and task.history with the project and task IDs above to inspect the current hold and attempts. Use task.retry after its reported cause is resolved and the task is eligible for a retry."
    );
    // Last, and cut to what the supervisor's mailbox accepts, so that a long
    // log never costs the notice itself.
    let notice = match check_log {
        Some(log) => crate::delivery_store::with_log(notice, &log),
        None => notice,
    };
    Ok((key, notice))
}

/// The once-only key of `supervisor.notify` in a project.
pub fn notify_key(identity: &str) -> Result<String> {
    digest(&("aq.delivery.supervisor_notify", identity))
}

/// Whether the project's supervisor was told under `key`; the row is locked
/// until the caller's transaction ends.
pub async fn told(db: &mut PgConnection, project: Uuid, key: &str) -> Result<bool> {
    let row: Option<Uuid> = sqlx::query_scalar("SELECT message_id FROM delivery_notifications WHERE project_id=$1 AND identity_digest=$2 FOR UPDATE")
        .bind(project)
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.is_some())
}

/// Records that `message` told the project's supervisor under `key`, in the
/// transaction that sends it.
pub async fn record_told(
    db: &mut PgConnection,
    project: Uuid,
    key: &str,
    message: Uuid,
    now_ms: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO delivery_notifications(project_id,identity_digest,message_id,notified_at_ms) VALUES($1,$2,$3,$4)")
        .bind(project)
        .bind(key)
        .bind(message)
        .bind(now_ms)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decided_result_answers_its_own_outcome_again_and_refuses_the_other() {
        let completed = Some(answer("completed"));
        assert_eq!(already("work.complete", Some("completed")), completed);
        assert_eq!(
            already("work.return", Some("returned")),
            Some(answer("returned"))
        );
        let stale = Some(refused("stale_result"));
        assert_eq!(already("work.complete", Some("returned")), stale);
        assert_eq!(already("work.return", Some("completed")), stale);
        for command in ["work.complete", "work.return"] {
            assert_eq!(already(command, Some("held")), None);
            assert_eq!(already(command, None), None);
        }
    }

    #[test]
    fn a_settlement_answers_once_it_is_known_and_stays_pending_otherwise() {
        let completed = settled("work.complete", Settled::Completed);
        assert_eq!(completed, Some((answer("completed"), Some("completed"))));
        let returned = settled("work.return", Settled::Returned);
        assert_eq!(returned, Some((answer("returned"), Some("returned"))));
        for command in ["work.complete", "work.return"] {
            let stale = settled(command, Settled::Stale);
            assert_eq!(stale, Some((refused("stale_result"), None)));
            assert_eq!(settled(command, Settled::Pending), None);
        }
        // A settlement that answers for the other command is never taken as this one's.
        assert_eq!(settled("work.complete", Settled::Returned), None);
        assert_eq!(settled("work.return", Settled::Completed), None);
    }

    #[test]
    fn the_notify_key_is_the_identity_alone() {
        let key = notify_key("result-1").unwrap();
        assert!(key.starts_with("sha256:") && key.len() == 71);
        assert_eq!(notify_key("result-1").unwrap(), key);
        assert_ne!(notify_key("result-2").unwrap(), key);
    }
}
