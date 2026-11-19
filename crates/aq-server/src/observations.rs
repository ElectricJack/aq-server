//! Durable AQ22 ledger adapter with separate human and worker HTTP surfaces.
//! Sources are derived from the trusted accounting catalog (`catalog`); source
//! registration is never a worker API.
use crate::{
    AppState,
    execution::authority::{self, AuthorityView, Kind, Policy as AuthorityPolicy},
};
use aq_accounting::{
    projection::Projection,
    reconcile::{self, Context, Decision, Fact, Policy, UnresolvedReason},
    wire::*,
};
use aq_observation_admission::{BatchReply, RefusalReason, Refusals};
use aq_row_retention::observation_sources as sources;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
pub mod api;
pub mod budget;
pub mod catalog;
pub mod inspection;
pub mod transport;
pub mod vm_time;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("observation scope or evidence refused")]
    Refused,
    #[error("observation identity conflicts")]
    Conflict,
    #[error("observation retained quota exhausted")]
    Capacity,
    /// A node observation limit from Settings was reached; the text says which.
    #[error("{0}")]
    Limit(String),
    #[error("observation database failed")]
    Database(#[from] sqlx::Error),
    #[error("observation authority failed")]
    Authority(#[from] authority::AuthorityError),
    #[error("invalid observation document")]
    Invalid,
}
pub type Result<T> = std::result::Result<T, Error>;
fn value<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|_| Error::Invalid)
}
fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|_| Error::Invalid)
}
fn integer(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| Error::Invalid)
}

pub use aq_accounting::source::OwnerSource;
pub use aq_row_retention::observation_sources::facts as freshness;
impl From<aq_accounting::Error> for Error {
    fn from(_: aq_accounting::Error) -> Self {
        Self::Invalid
    }
}
/// The stall sweep's call: releases the sources of ended attempts on every
/// node (`aq_row_retention::observation_sources`). Enrollment runs it for its node.
pub async fn release_ended(state: &AppState) -> Result<u64> {
    let mut conn = state.store.observation_pool.acquire().await?;
    let grace = aq_role_settings::observation_limits(&mut conn).await?.0;
    let now = (state.clock)().timestamp_millis();
    Ok(sources::release_ended(&mut conn, now, integer(grace.final_drain_grace_ms)?, None).await?)
}
fn node_of(binding: &Binding) -> Result<Uuid> {
    match binding.session {
        SessionIdentity::Remote { node_id, .. } => Ok(node_id),
        _ => Err(Error::Refused),
    }
}
fn verify_binding(view: &AuthorityView, binding: &Binding) -> Result<()> {
    let graph = view.graph.as_ref().ok_or(Error::Refused)?;
    let lineage =
        aq_admission::transport::task_lineage_id(view.project_id, graph.workspace_id, view.task_id)
            .map_err(|_| Error::Invalid)?;
    if view.kind != Kind::ProfiledV1
        || view.session != binding.session
        || view.project_id != binding.project_id
        || view.task_id != binding.task_id
        || view.attempt.attempt_id != binding.attempt_id
        || view.attempt.generation != binding.attempt_generation
        || lineage != binding.lineage_id
        || view.grant_id.is_none()
    {
        return Err(Error::Refused);
    }
    Ok(())
}
async fn live_node(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<()> {
    let revoked: Option<bool> =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await?;
    if revoked != Some(false) {
        return Err(Error::Refused);
    }
    Ok(())
}
/// Internal owner calls may use retained node identity. HTTP paths must use the
/// opaque authenticated variant so credential rotation is checked under locks.
enum NodeSubject<'a> {
    Trusted(Uuid),
    Authenticated(&'a crate::worker_api::AuthenticatedNode),
}
impl NodeSubject<'_> {
    fn id(&self) -> Uuid {
        match self {
            Self::Trusted(id) => *id,
            Self::Authenticated(context) => context.id(),
        }
    }
    async fn lock(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        match self {
            Self::Trusted(id) => live_node(tx, *id).await,
            Self::Authenticated(context) => crate::worker_api::lock_current_node(tx, context)
                .await
                .map(|_| ())
                .map_err(|_| Error::Refused),
        }
    }
}
/// Source admission requires a real active profiled grant. Replays after stop
/// may use only this retained exact authority; they cannot enroll new sources.
/// Enrolling a higher generation revokes the attempt's previous sources.
pub async fn register_source(state: &AppState, source: &OwnerSource) -> Result<()> {
    source.validate()?;
    let node = node_of(&source.binding)?;
    let mut tx = state.store.observation_pool.begin().await?;
    live_node(&mut tx, node).await?;
    let view = authority::load_locked(
        state,
        &mut tx,
        &source.binding.session,
        AuthorityPolicy::TerminalHistorical,
    )
    .await?;
    verify_binding(&view, &source.binding)?;
    // The source may not relabel the selected account/provider. Stable account
    // scope and native namespace are still supplied by the trusted owner catalog.
    let selected: Value = sqlx::query_scalar(
        "SELECT selection FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(source.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await?;
    let selected: aq_selection::ResolvedSelection = parse(selected)?;
    if source.policy.provider.as_str() != selected.choice.provider
        || source.account_claim.as_str() != selected.choice.account_reference
    {
        return Err(Error::Refused);
    }
    sqlx::query("INSERT INTO observation_nodes(node_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT retained_bytes,source_count FROM observation_nodes WHERE node_id=$1 FOR UPDATE",
    )
    .bind(node)
    .fetch_one(&mut *tx)
    .await?;
    let old = sources::stored(&mut tx, node, source.source_id, integer(source.generation)?).await?;
    let digest = source.digest()?;
    transport::require_active_locked(&mut tx, node, None).await?;
    if let Some((old, revoked)) = old {
        if old != digest.as_str() {
            return Err(Error::Conflict);
        }
        // A revoked source is never re-admitted, even with identical authority.
        if revoked {
            return Err(Error::Refused);
        }
        tx.commit().await?;
        return Ok(());
    }
    let SessionIdentity::Remote { boot_id, .. } = &source.binding.session else {
        return Err(Error::Refused);
    };
    transport::require_active_locked(&mut tx, node, Some(*boot_id)).await?;
    authority::load_locked(
        state,
        &mut tx,
        &source.binding.session,
        AuthorityPolicy::TerminalActive,
    )
    .await?;
    let bytes = serde_json::to_vec(source)
        .map_err(|_| Error::Invalid)?
        .len() as i64;
    let limits = aq_role_settings::observation_limits(&mut tx).await?.0;
    let grace = integer(limits.final_drain_grace_ms)?;
    // Ended attempts' sources are released first and no longer count.
    let now = (state.clock)().timestamp_millis();
    let live = sources::in_use_after_release(&mut tx, node, now, grace).await?;
    limits
        .admit_source(live, row.get("retained_bytes"), bytes)
        .map_err(Error::Limit)?;
    // A new generation supersedes every unrevoked source of the attempt, on any
    // node. Generations only grow, so a stale re-derivation cannot displace a
    // newer enrollment.
    let superseded = sources::unrevoked(&mut tx, source.binding.attempt_id).await?;
    let generation = integer(source.generation)?;
    if superseded.iter().any(|(_, _, g)| *g >= generation) {
        return Err(Error::Conflict);
    }
    sqlx::query("INSERT INTO observation_sources(node_id,source_id,generation,attempt_id,authority,authority_digest) VALUES($1,$2,$3,$4,$5,$6)").bind(node).bind(source.source_id).bind(generation).bind(source.binding.attempt_id).bind(value(source)?).bind(digest.as_str()).execute(&mut *tx).await?;
    sqlx::query("UPDATE observation_nodes SET source_count=source_count+1,retained_bytes=retained_bytes+$2 WHERE node_id=$1").bind(node).bind(bytes).execute(&mut *tx).await?;
    if !superseded.is_empty() {
        let revoked_at = (state.clock)().timestamp_millis();
        if revoked_at <= 0 {
            return Err(Error::Invalid);
        }
        let by = (node, source.source_id, generation);
        for old in superseded {
            sources::revoke(&mut tx, old, source.binding.attempt_id, by, revoked_at).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// `authenticated_node` must come from current node credential authentication.
/// The transaction checks revocation again after taking the node lock. This API
/// admits observations only; it never renews execution or releases a workspace.
pub async fn ingest(
    state: &AppState,
    authenticated_node: Uuid,
    packet: &Packet,
) -> Result<BatchReply> {
    ingest_subject(state, NodeSubject::Trusted(authenticated_node), packet).await
}
pub async fn ingest_authenticated(
    state: &AppState,
    context: &crate::worker_api::AuthenticatedNode,
    packet: &Packet,
) -> Result<BatchReply> {
    ingest_subject(state, NodeSubject::Authenticated(context), packet).await
}
/// A refused record refuses its source (durably, listed in the reply), never the batch.
async fn ingest_subject(
    state: &AppState,
    subject: NodeSubject<'_>,
    packet: &Packet,
) -> Result<BatchReply> {
    let authenticated_node = subject.id();
    packet.validate().map_err(|_| Error::Invalid)?;
    let Message::Batch(batch) = &packet.body.message else {
        return Err(Error::Refused);
    };
    if authenticated_node != batch.node_id {
        return Err(Error::Refused);
    }
    let mut tx = state.store.observation_pool.begin().await?;
    subject.lock(&mut tx).await?;
    // Stable ordering across all attempts, calls and aggregate rows in a batch.
    let sessions: BTreeMap<_, _> = batch
        .events
        .iter()
        .map(|e| (e.body.binding.attempt_id, &e.body.binding))
        .collect();
    let retention_enabled = {
        #[cfg(any(test, feature = "test-support"))]
        {
            state.store.fixture_profile.is_none_or(|v| v >= 64)
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            true
        }
    };
    let mut unbound = BTreeSet::new();
    for binding in sessions.values() {
        let closed = retention_enabled
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM retention_closed_attempts WHERE attempt_id=$1)",
            )
            .bind(binding.attempt_id)
            .fetch_one(&mut *tx)
            .await?;
        let policy = AuthorityPolicy::TerminalHistorical;
        let view = match authority::load_locked(state, &mut tx, &binding.session, policy).await {
            Err(authority::AuthorityError::Database(error)) => return Err(error.into()),
            view => view.ok(),
        };
        // Its sources are refused, below.
        if closed || view.is_none_or(|v| verify_binding(&v, binding).is_err()) {
            unbound.insert(binding.attempt_id);
        }
    }
    let row=sqlx::query("SELECT through_sequence,retained_bytes,event_count FROM observation_nodes WHERE node_id=$1 FOR UPDATE").bind(authenticated_node).fetch_optional(&mut *tx).await?.ok_or(Error::Refused)?;
    transport::require_active_locked(&mut tx, authenticated_node, None).await?;
    let old: Option<(Value, Option<Value>)> = sqlx::query_as(
        "SELECT receipt,refused FROM observation_packets WHERE node_id=$1 AND message_id=$2",
    )
    .bind(authenticated_node)
    .bind(packet.body.message_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((old, refused)) = old {
        let receipt: Receipt = parse(old)?;
        receipt
            .validate_against(packet)
            .map_err(|_| Error::Conflict)?;
        tx.commit().await?;
        let refused_sources = refused.map(parse).transpose()?.unwrap_or_default();
        return Ok(BatchReply::new(receipt, refused_sources));
    }
    if integer(batch.first_sequence)?
        != row
            .get::<i64, _>("through_sequence")
            .checked_add(1)
            .ok_or(Error::Capacity)?
    {
        return Err(Error::Conflict);
    }
    let recorded_at = (state.clock)().timestamp_millis();
    if recorded_at <= 0 {
        return Err(Error::Invalid);
    }
    let mut sources = BTreeMap::new();
    let mut keys = BTreeSet::new();
    let mut refused = Refusals::default();
    for event in &batch.events {
        let p = &event.body.provenance;
        let source_key = (p.source_id, p.source_generation);
        if refused.refused(event) {
            continue;
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = sources.entry(source_key) {
            // A source released at its attempt's end is gone after its final drain grace.
            let generation = integer(p.source_generation)?;
            let source_row = sources::for_ingest(
                &mut tx,
                authenticated_node,
                p.source_id,
                generation,
                recorded_at,
            )
            .await?;
            let stands = source_row.as_ref().map(|r| r.get::<bool, _>("refused"));
            let Some(source_row) = source_row.filter(|_| stands == Some(false)) else {
                let reason =
                    stands.map_or(RefusalReason::UnknownSource, |_| RefusalReason::Refused);
                refused.refuse(event, reason, false);
                continue;
            };
            let source: OwnerSource = parse(source_row.get("authority"))?;
            source.validate()?;
            if source.digest()?.as_str() != source_row.get::<String, _>("authority_digest") {
                return Err(Error::Invalid);
            }
            entry.insert((source, source_row.get::<bool, _>("revoked")));
        }
        let (source, _) = &sources[&source_key];
        let listed = (source.policy.parsers.iter()).any(|x| x.parser == p.parser);
        let bound = !unbound.contains(&event.body.binding.attempt_id);
        refused.check(
            event,
            bound,
            &source.binding,
            &source.native_session_key,
            listed,
        );
    }
    let events = refused.admitted(batch);
    for event in events.iter().copied() {
        let p = &event.body.provenance;
        let (source, _) = &sources[&(p.source_id, p.source_generation)];
        if event.body.kind.is_usage() {
            keys.insert(
                reconcile::key(event, &source.policy)
                    .map_err(|_| Error::Invalid)?
                    .as_str()
                    .to_owned(),
            );
        }
    }
    let current_attempts: BTreeSet<_> = events.iter().map(|e| e.body.binding.attempt_id).collect();
    let mut affected_attempts = current_attempts.clone();
    let mut updated_attempts = current_attempts.clone();
    for key in &keys {
        sqlx::query(
            "INSERT INTO observation_call_heads(call_key) VALUES($1) ON CONFLICT DO NOTHING",
        )
        .bind(key)
        .execute(&mut *tx)
        .await?;
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_attempt_id FROM observation_call_heads WHERE call_key=$1 FOR UPDATE",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(owner) = owner {
            affected_attempts.insert(owner);
        }
    }
    let lineages: BTreeSet<_> = events.iter().map(|e| e.body.binding.lineage_id).collect();
    let mut projections = BTreeMap::new();
    for lineage in lineages {
        sqlx::query("INSERT INTO observation_projections(lineage_id,projection) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(lineage).bind(value(&Projection::default())?).execute(&mut *tx).await?;
        let p: Value = sqlx::query_scalar(
            "SELECT projection FROM observation_projections WHERE lineage_id=$1 FOR UPDATE",
        )
        .bind(lineage)
        .fetch_one(&mut *tx)
        .await?;
        projections.insert(lineage, parse::<Projection>(p)?);
    }
    let mut attempt_projections = BTreeMap::new();
    for attempt in affected_attempts {
        if current_attempts.contains(&attempt) {
            sqlx::query("INSERT INTO observation_attempt_projections(attempt_id,projection) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(attempt).bind(value(&Projection::default())?).execute(&mut *tx).await?;
        }
        let projection: Value = sqlx::query_scalar(
            "SELECT projection FROM observation_attempt_projections WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await?;
        attempt_projections.insert(attempt, parse::<Projection>(projection)?);
    }
    let book = catalog::rate_book(state);
    let mut retained = 0i64;
    for event in events.iter().copied() {
        let (source, revoked) = &sources[&(
            event.body.provenance.source_id,
            event.body.provenance.source_generation,
        )];
        let mut appended = None;
        let projection = projections
            .get_mut(&event.body.binding.lineage_id)
            .ok_or(Error::Invalid)?;
        let decision = if let ObservationKind::Usage { usage } = &event.body.kind {
            let key = reconcile::key(event, &source.policy).map_err(|_| Error::Invalid)?;
            let (current, owner): (Option<Value>, Option<Uuid>) = sqlx::query_as(
                "SELECT fact,owner_attempt_id FROM observation_call_heads WHERE call_key=$1",
            )
            .bind(key.as_str())
            .fetch_one(&mut *tx)
            .await?;
            let current = current.map(parse::<Fact>).transpose()?;
            let target = if let Some(reference) = &usage.correction_of {
                let v: Option<Value> = sqlx::query_scalar(
                    "SELECT fact FROM observation_facts WHERE event_id=$1 AND call_key=$2",
                )
                .bind(reference.event_id)
                .bind(key.as_str())
                .fetch_optional(&mut *tx)
                .await?;
                v.map(parse::<Fact>).transpose()?
            } else {
                None
            };
            // Only token-rate sources pin a rate card. Flat subscription and
            // local usage is recorded but never priced at token rates.
            let pin = match source.billing {
                Some(EstimateBasis::TokenRates) => book
                    .current(&usage.call.provider, &usage.call.model)
                    .map(|card| card.reference.clone()),
                _ => None,
            };
            // Late usage from a superseded generation is kept as evidence but
            // never charged under its revoked policy, and leaves the lineage
            // accounting incomplete rather than stalling the node sequence.
            let decision = if *revoked || usage.call.account_reference != source.account_claim {
                Decision::Unresolved {
                    reason: UnresolvedReason::AttributionChanged,
                }
            } else {
                match reconcile::decide(
                    event,
                    Context {
                        lineage: source.binding.lineage_id,
                        policy: &source.policy,
                        current: current.as_ref(),
                        correction_target: target.as_ref(),
                        baseline: None,
                        pricing_reference: pin.as_ref(),
                    },
                ) {
                    Ok(v) => v,
                    Err(aq_accounting::Error::Conflict | aq_accounting::Error::Unsupported) => {
                        Decision::Unresolved {
                            reason: UnresolvedReason::AttributionChanged,
                        }
                    }
                    Err(_) => return Err(Error::Invalid),
                }
            };
            match &decision {
                Decision::Append { fact, .. } => {
                    *projection = projection
                        .replace_fact(current.as_ref(), fact)
                        .map_err(|_| Error::Capacity)?;
                    let owner = owner.unwrap_or(event.body.binding.attempt_id);
                    updated_attempts.insert(owner);
                    let owned = attempt_projections.get_mut(&owner).ok_or(Error::Invalid)?;
                    *owned = owned
                        .replace_fact(current.as_ref(), fact)
                        .map_err(|_| Error::Capacity)?;
                    sqlx::query("UPDATE observation_call_heads SET fact=$2,owner_attempt_id=$3,lineage_id=$4 WHERE call_key=$1")
                        .bind(key.as_str())
                        .bind(value(fact)?)
                        .bind(owner)
                        .bind(fact.lineage)
                        .execute(&mut *tx)
                        .await?;
                    appended = Some((key, fact.clone()));
                }
                Decision::Unresolved { .. } => {
                    projection.unresolved_events = projection
                        .unresolved_events
                        .checked_add(1)
                        .ok_or(Error::Capacity)?;
                    let owned = attempt_projections
                        .get_mut(&event.body.binding.attempt_id)
                        .ok_or(Error::Invalid)?;
                    owned.unresolved_events = owned
                        .unresolved_events
                        .checked_add(1)
                        .ok_or(Error::Capacity)?;
                }
                Decision::NoCharge { .. } => {}
            }
            value(&decision)?
        } else {
            if matches!(event.body.kind, ObservationKind::Gap { .. }) {
                projection.unresolved_events = projection
                    .unresolved_events
                    .checked_add(1)
                    .ok_or(Error::Capacity)?;
                let owned = attempt_projections
                    .get_mut(&event.body.binding.attempt_id)
                    .ok_or(Error::Invalid)?;
                owned.unresolved_events = owned
                    .unresolved_events
                    .checked_add(1)
                    .ok_or(Error::Capacity)?;
            }
            json!({"kind":"observation_only"})
        };
        retained = retained
            .checked_add(serde_json::to_vec(event).map_err(|_| Error::Invalid)?.len() as i64)
            .and_then(|v| v.checked_add(decision.to_string().len() as i64))
            .ok_or(Error::Capacity)?;
        sqlx::query("INSERT INTO observation_events(event_id,node_id,sequence,lineage_id,attempt_id,record,decision,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(event.event_id).bind(authenticated_node).bind(integer(event.body.sequence)?).bind(event.body.binding.lineage_id).bind(event.body.binding.attempt_id).bind(value(event)?).bind(decision).bind(recorded_at).execute(&mut *tx).await?;
        if let Some((key, fact)) = appended {
            retained = retained
                .checked_add(
                    (serde_json::to_vec(&fact).map_err(|_| Error::Invalid)?.len() * 2) as i64,
                )
                .ok_or(Error::Capacity)?;
            sqlx::query("INSERT INTO observation_facts(event_id,call_key,fact) VALUES($1,$2,$3)")
                .bind(event.event_id)
                .bind(key.as_str())
                .bind(value(&fact)?)
                .execute(&mut *tx)
                .await?;
        }
    }
    let receipt = Receipt {
        schema_version: SCHEMA_VERSION,
        message_id: packet.body.message_id,
        body_digest: packet.body_digest.clone(),
        disposition: ReceiptDisposition::ObservationRecorded,
        through_sequence: Some(batch.last_sequence),
    };
    receipt
        .validate_against(packet)
        .map_err(|_| Error::Invalid)?;
    let refused_sources = refused.list();
    let refused_value = (!refused_sources.is_empty())
        .then(|| value(&refused_sources))
        .transpose()?;
    for (source_id, generation, reason, sequence) in refused.recorded(batch) {
        sqlx::query("INSERT INTO observation_source_refusals(node_id,source_id,generation,reason,message_id,sequence,refused_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(authenticated_node).bind(source_id).bind(integer(generation)?).bind(reason).bind(packet.body.message_id).bind(integer(sequence)?).bind(recorded_at).execute(&mut *tx).await?;
    }
    retained = retained
        .checked_add(
            refused_value
                .as_ref()
                .map_or(0, |v| v.to_string().len() as i64),
        )
        .ok_or(Error::Capacity)?;
    retained = retained
        .checked_add(
            serde_json::to_vec(&receipt)
                .map_err(|_| Error::Invalid)?
                .len() as i64
                + 4096 * (projections.len() + attempt_projections.len()) as i64,
        )
        .ok_or(Error::Capacity)?;
    let limits = aq_role_settings::observation_limits(&mut tx).await?.0;
    limits
        .admit_events(
            row.get("event_count"),
            events.len() as i64,
            row.get("retained_bytes"),
            retained,
        )
        .map_err(Error::Limit)?;
    let progressed: BTreeSet<_> = events
        .iter()
        .filter(|e| crate::stall::evidence::is_progress(&e.body.kind))
        .map(|e| e.body.binding.attempt_id)
        .collect();
    for (lineage, projection) in projections {
        projection.validate().map_err(|_| Error::Invalid)?;
        sqlx::query("UPDATE observation_projections SET revision=revision+1,projection=$2,last_recorded_at_ms=GREATEST(last_recorded_at_ms,$3) WHERE lineage_id=$1").bind(lineage).bind(value(&projection)?).bind(recorded_at).execute(&mut *tx).await?;
    }
    for (attempt, projection) in attempt_projections {
        if !updated_attempts.contains(&attempt) {
            continue;
        }
        projection.validate().map_err(|_| Error::Invalid)?;
        let at = |seen: bool| if seen { recorded_at } else { 0 };
        sqlx::query("UPDATE observation_attempt_projections SET revision=revision+1,projection=$2,last_recorded_at_ms=GREATEST(last_recorded_at_ms,$3),last_observed_at_ms=GREATEST(last_observed_at_ms,$4),last_progress_at_ms=GREATEST(last_progress_at_ms,$5) WHERE attempt_id=$1").bind(attempt).bind(value(&projection)?).bind(recorded_at).bind(at(current_attempts.contains(&attempt))).bind(at(progressed.contains(&attempt))).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO observation_packets(node_id,message_id,body_digest,receipt,refused) VALUES($1,$2,$3,$4,$5)").bind(authenticated_node).bind(packet.body.message_id).bind(packet.body_digest.as_str()).bind(value(&receipt)?).bind(refused_value).execute(&mut *tx).await?;
    if retention_enabled {
        sqlx::query("INSERT INTO observation_packet_retention(node_id,message_id,first_sequence,last_sequence,attempt_ids,recorded_at_ms,charged_bytes) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(authenticated_node).bind(packet.body.message_id).bind(integer(batch.first_sequence)?).bind(integer(batch.last_sequence)?)
        .bind(current_attempts.iter().copied().collect::<Vec<_>>()).bind(recorded_at).bind(retained).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE observation_nodes SET through_sequence=$2,retained_bytes=retained_bytes+$3,event_count=event_count+$4 WHERE node_id=$1").bind(authenticated_node).bind(integer(batch.last_sequence)?).bind(retained).bind(events.len() as i64).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(BatchReply::new(receipt, refused_sources))
}
