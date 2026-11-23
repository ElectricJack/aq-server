//! Replacement of a proven-failed native attempt by a new attempt, session and
//! generation. A replacement is admitted only after stop proof, retained source
//! and quiescent effects; uncertain prior execution always blocks. Releasing the
//! predecessor's task claim is a durable, immutable owner record, never inferred.
use crate::{AppState, error::AppError};
use aq_admission::ArtifactRef;
pub use aq_profiled_state::replacement::*;
use aq_profiled_state::replacement_sql as sql;
pub use sql::predecessor_project_revision;
use uuid::Uuid;

/// Loads started-attempt owner facts. A history that does not match the
/// central started count (for example legacy attempts) is returned as `None`.
pub async fn load(
    conn: &mut sqlx::PgConnection,
    task: Uuid,
    started: u32,
) -> Result<Option<Vec<Prior>>, AppError> {
    let priors = sql::load(conn, task).await.map_err(internal)?;
    Ok((priors.len() == started as usize).then_some(priors))
}

fn internal<E>(_: E) -> AppError {
    AppError::Internal
}
/// The task's retry budget ([`Limits::of`]): the limits of the selection policy its latest
/// attempt pinned, else the current Settings values. Nothing here is a server constant.
pub async fn limits(conn: &mut sqlx::PgConnection, priors: &[Prior]) -> Result<Limits, AppError> {
    let pinned = match latest(priors) {
        Some(attempt) => super::continuation_policy::pinned_policy(conn, attempt).await?,
        None => None,
    };
    let settings = (aq_role_settings::selection_defaults(conn).await).map_err(internal)?;
    Ok(Limits::of(pinned.as_ref(), &settings))
}

/// Scheduling assessment for one Graph-ready task.
/// The decision attempt counts alone settle: an exhausted never-granted
/// retry bound blocks, and no started attempt is an ordinary first attempt.
pub fn precheck(counts: &super::no_grant::AttemptCounts) -> Option<Assessment> {
    if counts.never_granted_bound_exhausted() {
        return Some(Assessment::Blocked("never_granted_retry_bound_exhausted"));
    }
    (counts.started == 0).then_some(Assessment::Fresh)
}
pub async fn assess_task(state: &AppState, task: Uuid, now: u64) -> Result<Assessment, AppError> {
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    assess_task_in(&mut conn, task, now).await
}
/// [`assess_task`] reading every fact on `conn` (a caller's transaction sees its own facts and
/// holds no second pool connection, #117 review).
pub async fn assess_task_in(
    conn: &mut sqlx::PgConnection,
    task: Uuid,
    now: u64,
) -> Result<Assessment, AppError> {
    assess_on(conn, task, now, assess).await
}

async fn assess_on(
    conn: &mut sqlx::PgConnection,
    task: Uuid,
    now: u64,
    decide: fn(&[Prior], &Limits, u64) -> Assessment,
) -> Result<Assessment, AppError> {
    if super::disposition::task_disposed(conn, task).await? {
        return Ok(Assessment::Blocked("task_terminally_disposed"));
    }
    // H3 rung 5: a quarantined lineage is never replaced until an owner acts.
    if sql::lineage_quarantined(conn, task)
        .await
        .map_err(internal)?
    {
        return Ok(Assessment::Blocked("stall_quarantined"));
    }
    // D1: replacing an attempt with the same immutable export repeats a failed
    // legacy publication. That lineage needs a new source admission.
    if sql::delivery_unpublished(conn, task)
        .await
        .map_err(internal)?
    {
        return Ok(Assessment::Blocked(
            "replacement_after_unpublished_delivery_needs_new_source",
        ));
    }
    let counts = super::no_grant::attempt_counts_locked(conn, task).await?;
    if let Some(decided) = precheck(&counts) {
        return Ok(decided);
    }
    Ok(match load(conn, task, counts.started).await? {
        Some(priors) => decide(&priors, &limits(conn, &priors).await?, now),
        None => Assessment::Blocked("replacement_lineage_history_unknown"),
    })
}

/// Scheduling assessment against the current project binding revision. A
/// successor reuses the predecessor's export, whose request pins the project
/// revision it was offered under; source observation refuses any other
/// revision. Such a lineage is reported blocked here, so one unreplaceable
/// task cannot fail the whole poll and starve the project's other tasks.
pub async fn assess_task_for_revision(
    state: &AppState,
    task: Uuid,
    now: u64,
    project_revision: u64,
) -> Result<Assessment, AppError> {
    let assessment = assess_task(state, task, now).await?;
    if let Assessment::Blocked(code) = assessment
        && aq_machine_control::holds::is_budget_hold(code)
    {
        let mut conn = state.store.pool.acquire().await.map_err(internal)?;
        let closing = assess_on(&mut conn, task, now, assess_for_fresh_start).await?;
        let fenced = sql::fence_revision(&mut *conn, closing, project_revision)
            .await
            .map_err(internal)?;
        if matches!(fenced, Assessment::Blocked(aq_machine_control::fresh::HOLD)) {
            return Ok(fenced);
        }
    }
    sql::fence_revision(&state.store.pool, assessment, project_revision)
        .await
        .map_err(internal)
}
/// The hold the offer path records for `task` ([`assess_task_for_revision`]), if any: why an
/// owner retry cannot simply release it.
pub async fn retry_hold(
    state: &AppState,
    task: Uuid,
    now: u64,
    revision: u64,
) -> Result<Option<&'static str>, AppError> {
    Ok(
        match assess_task_for_revision(state, task, now, revision).await? {
            Assessment::Blocked(code) if aq_machine_control::holds::is_hold(code) => Some(code),
            _ => None,
        },
    )
}

/// An owner's explicit retry overrides the limit hold `code` of `task` (owner decision
/// 2026-11-18) and assesses it again, in one transaction: the override is recorded only when
/// the task can then be offered, so it is never one the offer path would still refuse. Every
/// budget starts again; what happened to the task refuses as before.
pub async fn override_budget(
    state: &AppState,
    (project, task): (Uuid, Uuid),
    code: &'static str,
    mut o: aq_machine_control::holds::Override,
    project_revision: u64,
) -> Result<Assessment, AppError> {
    use aq_machine_control::holds;
    let at = (state.clock)();
    let now = u64::try_from(o.recorded_at_ms).map_err(internal)?;
    let mut tx = state.store.pool.begin().await.map_err(internal)?;
    let known =
        holds::is_budget_hold(code) && holds::has_overrides(&mut tx).await.map_err(internal)?;
    let latest = holds::latest_attempt(&mut tx, task)
        .await
        .map_err(internal)?;
    let Some(attempt) = latest.filter(|_| known) else {
        return Ok(Assessment::Blocked(code));
    };
    (o.attempt_id, o.hold_code) = (attempt, code.into());
    (holds::record_override(&mut tx, task, &o, at).await).map_err(internal)?;
    let again = assess_task_in(&mut tx, task, now).await?;
    let again = sql::fence_revision(&mut *tx, again, project_revision)
        .await
        .map_err(internal)?;
    if !matches!(again, Assessment::Blocked(_)) {
        tx.commit().await.map_err(internal)?;
        tracing::info!(%task, hold = code, actor = o.actor, "owner overrode a limit hold; the task's budgets start again");
        holds::post_override(
            &state.store.pool,
            aq_task_comments::Thread {
                project_id: project,
                task_id: task,
            },
            &o,
            at,
        )
        .await;
    }
    Ok(again)
}

/// Releases the failed predecessor's task claim under the common node →
/// attempt/hot locks, after re-assessing the locked history. Idempotent.
pub async fn release(state: &AppState, task: Uuid, r: &Replacement) -> Result<(), AppError> {
    release_as(state, task, r, None).await
}
/// Verify the predecessor when a changed project binding requires a new lineage.
pub async fn fresh_predecessor(
    state: &AppState,
    task: Uuid,
    now: u64,
) -> Result<Assessment, AppError> {
    let mut conn = state.store.pool.acquire().await.map_err(internal)?;
    assess_on(&mut conn, task, now, assess_for_fresh_start).await
}

pub async fn release_as(
    state: &AppState,
    task: Uuid,
    replacement: &Replacement,
    fresh: Option<&aq_machine_control::fresh::FreshStart>,
) -> Result<(), AppError> {
    if !replacement.needs_release && fresh.is_none() {
        return Ok(());
    }
    let db = |_: sqlx::Error| AppError::Internal;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sql::lock_release(&mut tx, task, replacement.predecessor)
        .await
        .map_err(db)?;
    let counts = super::no_grant::attempt_counts_locked(&mut tx, task).await?;
    let priors = load(&mut tx, task, counts.started)
        .await?
        .ok_or_else(|| AppError::Conflict("replacement history changed".into()))?;
    let now = u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(AppError::Internal)?;
    let limits = limits(&mut tx, &priors).await?;
    let decision = if fresh.is_some() {
        assess_for_fresh_start
    } else {
        assess
    };
    let Assessment::Replace(locked) = decision(&priors, &limits, now) else {
        return Err(AppError::Conflict(
            "replacement no longer admissible".into(),
        ));
    };
    if let Some(f) = fresh
        && sql::predecessor_project_revision(&mut *tx, locked.predecessor)
            .await
            .map_err(internal)?
            == Some(f.project_revision)
    {
        return Err(AppError::Conflict(
            "predecessor already uses this binding".into(),
        ));
    }
    if locked.predecessor != replacement.predecessor {
        return Err(AppError::Conflict("replacement predecessor changed".into()));
    }
    if locked.needs_release {
        sql::record_release(&mut tx, task, &locked, now)
            .await
            .map_err(internal)?;
    }
    if let Some(f) = fresh {
        let at = (state.clock)();
        let recorded = aq_machine_control::fresh::record(
            &mut tx,
            aq_machine_control::fresh::ReleasedAttempt {
                task,
                released: locked.predecessor,
            },
            f,
            at,
        );
        recorded.await.map_err(internal)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(())
}

/// Read the immutable release receipt before selecting the predecessor's owned
/// export. A missing or mismatched release must never fall back to fresh source.
pub(crate) async fn retained_source_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: Uuid,
    predecessor: Uuid,
    receipt: &ArtifactRef,
) -> Result<super::source_exports::LinkedExport, AppError> {
    let digest: Option<String> = sqlx::query_scalar(
        "SELECT retained_source_receipt_digest FROM profiled_replacement_releases WHERE attempt_id=$1 AND task_id=$2 FOR SHARE",
    )
    .bind(predecessor)
    .bind(task)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    if digest.as_deref() != Some(receipt.digest.as_str()) {
        return Err(AppError::Conflict(
            "replacement retained source receipt changed or missing".into(),
        ));
    }
    super::source_exports::load_linked_locked(tx, predecessor).await
}

pub async fn retained_source(
    state: &AppState,
    task: Uuid,
    replacement: &Replacement,
) -> Result<super::source_exports::LinkedExport, AppError> {
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    let linked = retained_source_locked(
        &mut tx,
        task,
        replacement.predecessor,
        &replacement.retained_source_receipt,
    )
    .await?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn replacement_sql_matches_migrated_schema() {
        let db = crate::test_support::TestDb::create().await;
        let store = db.store().await;
        // The task has no started attempts: the history query plans and runs.
        let mut conn = store.pool.acquire().await.unwrap();
        let none = load(&mut conn, Uuid::new_v4(), 0).await.unwrap();
        assert_eq!(none.map(|priors| priors.len()), Some(0));
        // The guard body plans against the real schema and refuses a release
        // for an attempt that does not exist, before any foreign key check.
        let digest = "a".repeat(64);
        let err = sqlx::query("INSERT INTO profiled_replacement_releases VALUES($1,$2,'failed_attempt_replacement',$3,$3,$3,1,1)")
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(&digest)
            .execute(&store.pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("replacement release requires"), "{err}");
    }
}
