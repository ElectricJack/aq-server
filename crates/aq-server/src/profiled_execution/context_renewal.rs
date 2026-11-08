//! Renewed context facts (spec 0007 §7.3). A node's measurement renews a
//! configured qualification for 24 h from `EFFECTIVE_DELAY_MS` later; offer
//! budgeting and the swarm read use the merged facts. Validation is pure
//! (`aq-context-renewal`); only renewals of a currently configured fact count.
use super::context_budget::{self, CountedStartup, Qualification};
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_context_renewal::{effective, merged, pending, renew_all};
use aq_protocol::{ArtifactPin, MeasurementSubmission, QualificationPin};

pub const EFFECTIVE_DELAY_MS: u64 = 60_000;
fn db<E>(_: E) -> AppError {
    AppError::Internal
}
fn window(f: &Qualification) -> (u64, u64) {
    (f.observed_at_ms, f.expires_at_ms)
}
type Renewals = Vec<(usize, Qualification)>;
/// The configured facts and the newest effective and newest pending renewal of each.
async fn load(state: &AppState, now_ms: u64) -> Result<(Vec<Qualification>, Renewals), AppError> {
    let Some(config) = state.config.profiled_execution.as_ref() else {
        return Ok(Default::default());
    };
    let configured = config.context_qualifications.clone();
    let mut conn = state.store.pool.acquire().await.map_err(db)?;
    if !aq_machine_control::has_format(&mut conn).await? {
        return Ok((configured, vec![]));
    }
    let bases = configured
        .iter()
        .map(|f| f.artifact().map(|a| a.digest.as_str().to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as("SELECT DISTINCT ON (base_renderer_digest, observed_at_ms <= $1) base_renderer_digest, fact FROM context_qualification_renewals WHERE expires_at_ms > $1 AND base_renderer_digest = ANY($2) ORDER BY base_renderer_digest, observed_at_ms <= $1, observed_at_ms DESC")
        .bind(i64::try_from(now_ms).map_err(db)?).bind(&bases).fetch_all(&mut *conn).await.map_err(db)?;
    let renewals = rows
        .into_iter()
        .filter_map(|(base, fact)| {
            let fact = serde_json::from_value::<Qualification>(fact).ok()?;
            fact.validate().ok()?;
            Some((bases.iter().position(|b| *b == base)?, fact))
        })
        .collect();
    Ok((configured, renewals))
}
/// One fact per configured fact: its newest renewal effective at `now_ms`, else
/// the configured fact. Only configured facts below store format 101.
pub async fn current_facts(state: &AppState, now_ms: u64) -> Result<Vec<Qualification>, AppError> {
    let (configured, renewals) = load(state, now_ms).await?;
    Ok(merged(&configured, &renewals, window, now_ms))
}
/// The newest not-yet-effective renewal (so services can prepare the switch),
/// else the first effective current fact; None without one.
pub async fn current_pin(
    state: &AppState,
    now_ms: u64,
) -> Result<Option<QualificationPin>, AppError> {
    let (configured, renewals) = load(state, now_ms).await?;
    let current = merged(&configured, &renewals, window, now_ms);
    let fact = pending(&renewals, window, now_ms)
        .or_else(|| current.iter().find(|f| effective(window(f), now_ms)));
    fact.map(pin).transpose()
}
fn pin(f: &Qualification) -> Result<QualificationPin, AppError> {
    let a = f.artifact().map_err(db)?;
    let renderer = ArtifactPin {
        id: a.id.as_str().into(),
        version: a.version.as_str().into(),
        digest: a.digest.as_str().into(),
    };
    Ok(QualificationPin {
        renderer,
        effective_at_ms: f.observed_at_ms,
        expires_at_ms: f.expires_at_ms,
    })
}
/// Startup counting over the current facts. A refusal logs its reason at warn.
pub(super) async fn evaluate(
    state: &AppState,
    runtime: &super::registration::RegisteredRuntime,
    selected: &aq_selection::ResolvedSelection,
    composition: &aq_profiles::FrozenComposition,
    now_ms: u64,
) -> Result<(CountedStartup, Qualification), AppError> {
    let facts = current_facts(state, now_ms).await?;
    let counted = context_budget::evaluate(&facts, runtime, selected, composition, now_ms)
        .map_err(|e| {
            tracing::warn!(reason = %e, "no native offer: the startup context budget refused");
            super::producer::refused()
        })?;
    let fact = facts
        .into_iter()
        .find(|f| f.artifact().ok().as_ref() == Some(&counted.proof.qualification));
    Ok((counted, fact.ok_or_else(|| super::producer::refused())?))
}
/// Renews every configured fact whose credential allows the node and whose
/// counting evidence the submission satisfies; returns the first renewed pin.
pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    submission: &MeasurementSubmission,
) -> Result<QualificationPin, AppError> {
    if submission.schema_version != 1 {
        return Err(AppError::BadRequest("schema_version".into()));
    }
    let none = || AppError::Conflict("no configured qualification this node may renew".into());
    let (Some(config), Some(registry)) = (
        state.config.profiled_execution.as_ref(),
        state.config.intelligence.as_deref(),
    ) else {
        return Err(none());
    };
    let now_ms = u64::try_from((state.clock)().timestamp_millis()).map_err(db)?;
    let bases: Vec<&Qualification> = (config.context_qualifications.iter())
        .filter(|b| {
            (registry.config.credentials.iter()).any(|c| {
                c.credential.profile == b.subject.credential_profile && c.allows(node.id())
            })
        })
        .collect();
    let body = |b: &Qualification| {
        config
            .definition_bodies
            .iter()
            .find(|d| d.artifact() == &b.counting_evidence)
            .map(|d| d.body())
    };
    let inputs = bases
        .iter()
        .map(|b| Ok((serde_json::to_value(b)?, body(b))))
        .collect::<Result<Vec<_>, serde_json::Error>>()
        .map_err(db)?;
    let renewed = renew_all(&inputs, submission, now_ms + EFFECTIVE_DELAY_MS)
        .map_err(|e| e.map_or_else(none, |e| AppError::BadRequest(e.to_string())))?;
    let renewed = (renewed.into_iter())
        .map(|(i, fact)| {
            let fact: Qualification = serde_json::from_value(fact).map_err(db)?;
            Ok((
                bases[i].artifact().map_err(db)?,
                fact.artifact().map_err(db)?,
                fact,
            ))
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let submitted = serde_json::to_value(submission).map_err(db)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("DELETE FROM context_qualification_renewals WHERE expires_at_ms <= $1")
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let bases: Vec<&str> = renewed.iter().map(|(b, _, _)| b.digest.as_str()).collect();
    let delay = EFFECTIVE_DELAY_MS;
    if let Some(why) =
        aq_machine_control::renewal_hold(&mut tx, node.id(), &bases, now_ms, delay).await?
    {
        return Err(AppError::Conflict(why.into()));
    }
    for (base, renderer, fact) in &renewed {
        sqlx::query("INSERT INTO context_qualification_renewals(renderer_digest,base_renderer_digest,fact,observed_at_ms,expires_at_ms,node_id,submission,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,now()) ON CONFLICT (renderer_digest) DO NOTHING")
            .bind(renderer.digest.as_str()).bind(base.digest.as_str()).bind(serde_json::to_value(fact).map_err(db)?)
            .bind(fact.observed_at_ms as i64).bind(fact.expires_at_ms as i64).bind(node.id()).bind(&submitted)
            .execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    pin(&renewed[0].2)
}
