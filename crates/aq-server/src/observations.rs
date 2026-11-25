//! Durable AQ22 ledger adapter with separate human and worker HTTP surfaces.
//! Sources are derived from the trusted accounting catalog (`catalog`); source
//! registration is never a worker API.
use crate::{
    AppState,
    execution::authority::{self, AuthorityView, Kind, Policy as AuthorityPolicy},
};
use aq_accounting::{
    projection::Projection,
    reconcile::{Decision, Fact, Policy},
    wire::*,
};
use aq_observation_admission::{BatchReply, RefusalReason, Refusals};
use aq_observation_ledger::{BatchInput, BatchProjections, BatchSources};
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
impl From<aq_observation_ledger::Error> for Error {
    fn from(error: aq_observation_ledger::Error) -> Self {
        match error {
            aq_observation_ledger::Error::Invalid => Self::Invalid,
            aq_observation_ledger::Error::Capacity => Self::Capacity,
            aq_observation_ledger::Error::Database(error) => Self::Database(error),
        }
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
    let same_task = (view.project_id, view.task_id, lineage)
        == (binding.project_id, binding.task_id, binding.lineage_id);
    let same_attempt = (view.attempt.attempt_id, view.attempt.generation)
        == (binding.attempt_id, binding.attempt_generation);
    let granted_session =
        view.kind == Kind::ProfiledV1 && view.session == binding.session && view.grant_id.is_some();
    (same_task && same_attempt && granted_session)
        .then_some(())
        .ok_or(Error::Refused)
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
    verify_source_selection(&mut tx, source).await?;
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
    let enrollment = Enrollment {
        state,
        node,
        source,
    };
    enrollment
        .enroll(&mut tx, row.get("retained_bytes"), &digest)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn verify_source_selection(
    tx: &mut Transaction<'_, Postgres>,
    source: &OwnerSource,
) -> Result<()> {
    // The source may not relabel the selected account/provider. Stable account
    // scope and native namespace are still supplied by the trusted owner catalog.
    let selected: Value = sqlx::query_scalar(
        "SELECT selection FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(source.binding.attempt_id)
    .fetch_one(&mut **tx)
    .await?;
    let selected: aq_selection::ResolvedSelection = parse(selected)?;
    if source.policy.provider.as_str() != selected.choice.provider
        || source.account_claim.as_str() != selected.choice.account_reference
    {
        return Err(Error::Refused);
    }
    Ok(())
}

struct Enrollment<'a> {
    state: &'a AppState,
    node: Uuid,
    source: &'a OwnerSource,
}

impl Enrollment<'_> {
    async fn admit_size(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        retained_bytes: i64,
    ) -> Result<i64> {
        let Self {
            state,
            node,
            source,
        } = *self;
        let bytes = serde_json::to_vec(source)
            .map_err(|_| Error::Invalid)?
            .len() as i64;
        let limits = aq_role_settings::observation_limits(tx).await?.0;
        let grace = integer(limits.final_drain_grace_ms)?;
        // Ended attempts' sources are released first and no longer count.
        let now = (state.clock)().timestamp_millis();
        let live = sources::in_use_after_release(tx, node, now, grace).await?;
        limits
            .admit_source(live, retained_bytes, bytes)
            .map_err(Error::Limit)?;
        Ok(bytes)
    }

    async fn enroll(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        retained_bytes: i64,
        digest: &Digest,
    ) -> Result<()> {
        let Self {
            state,
            node,
            source,
        } = *self;
        let SessionIdentity::Remote { boot_id, .. } = &source.binding.session else {
            return Err(Error::Refused);
        };
        transport::require_active_locked(tx, node, Some(*boot_id)).await?;
        authority::load_locked(
            state,
            tx,
            &source.binding.session,
            AuthorityPolicy::TerminalActive,
        )
        .await?;
        let bytes = self.admit_size(tx, retained_bytes).await?;
        // A new generation supersedes every unrevoked source of the attempt, on any
        // node. Generations only grow, so a stale re-derivation cannot displace a
        // newer enrollment.
        let superseded = sources::unrevoked(tx, source.binding.attempt_id).await?;
        let generation = integer(source.generation)?;
        if superseded.iter().any(|(_, _, g)| *g >= generation) {
            return Err(Error::Conflict);
        }
        sqlx::query("INSERT INTO observation_sources(node_id,source_id,generation,attempt_id,authority,authority_digest) VALUES($1,$2,$3,$4,$5,$6)").bind(node).bind(source.source_id).bind(generation).bind(source.binding.attempt_id).bind(value(source)?).bind(digest.as_str()).execute(&mut **tx).await?;
        sqlx::query("UPDATE observation_nodes SET source_count=source_count+1,retained_bytes=retained_bytes+$2 WHERE node_id=$1").bind(node).bind(bytes).execute(&mut **tx).await?;
        if !superseded.is_empty() {
            let revoked_at = (state.clock)().timestamp_millis();
            if revoked_at <= 0 {
                return Err(Error::Invalid);
            }
            let supersession = sources::Supersession {
                attempt: source.binding.attempt_id,
                by: sources::SourceKey {
                    node,
                    source: source.source_id,
                    generation,
                },
                at_ms: revoked_at,
            };
            for old in superseded {
                supersession.revoke(tx, old).await?;
            }
        }
        Ok(())
    }
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
async fn unbound_attempts(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    batch: &ObservationBatch,
    retention_enabled: bool,
) -> Result<BTreeSet<Uuid>> {
    // Stable ordering across all attempts, calls and aggregate rows in a batch.
    let sessions: BTreeMap<_, _> = batch
        .events
        .iter()
        .map(|e| (e.body.binding.attempt_id, &e.body.binding))
        .collect();
    let mut unbound = BTreeSet::new();
    for binding in sessions.values() {
        let closed = retention_enabled
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM retention_closed_attempts WHERE attempt_id=$1)",
            )
            .bind(binding.attempt_id)
            .fetch_one(&mut **tx)
            .await?;
        let policy = AuthorityPolicy::TerminalHistorical;
        let view = match authority::load_locked(state, tx, &binding.session, policy).await {
            Err(authority::AuthorityError::Database(error)) => return Err(error.into()),
            view => view.ok(),
        };
        // Its sources are refused, below.
        if closed || view.is_none_or(|v| verify_binding(&v, binding).is_err()) {
            unbound.insert(binding.attempt_id);
        }
    }
    Ok(unbound)
}

async fn batch_sources(
    tx: &mut Transaction<'_, Postgres>,
    batch: &ObservationBatch,
    unbound: &BTreeSet<Uuid>,
    recorded_at: i64,
) -> Result<(BatchSources, Refusals)> {
    let mut sources = BTreeMap::new();
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
                tx,
                sources::SourceKey {
                    node: batch.node_id,
                    source: p.source_id,
                    generation,
                },
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
            bound.then_some(aq_observation_admission::EnrolledSource {
                binding: &source.binding,
                session_key: &source.native_session_key,
                parser_listed: listed,
            }),
        );
    }
    Ok((sources, refused))
}

async fn recorded_receipt(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    packet: &Packet,
) -> Result<Option<(Receipt, Option<Value>)>> {
    let old: Option<(Value, Option<Value>)> = sqlx::query_as(
        "SELECT receipt,refused FROM observation_packets WHERE node_id=$1 AND message_id=$2",
    )
    .bind(node)
    .bind(packet.body.message_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((old, refused)) = old else {
        return Ok(None);
    };
    let receipt: Receipt = parse(old)?;
    receipt
        .validate_against(packet)
        .map_err(|_| Error::Conflict)?;
    Ok(Some((receipt, refused)))
}

fn packet_batch(packet: &Packet, authenticated_node: Uuid) -> Result<&ObservationBatch> {
    packet.validate().map_err(|_| Error::Invalid)?;
    let Message::Batch(batch) = &packet.body.message else {
        return Err(Error::Refused);
    };
    if authenticated_node != batch.node_id {
        return Err(Error::Refused);
    }
    Ok(batch)
}

/// A refused record refuses its source (durably, listed in the reply), never the batch.
async fn ingest_subject(
    state: &AppState,
    subject: NodeSubject<'_>,
    packet: &Packet,
) -> Result<BatchReply> {
    let authenticated_node = subject.id();
    let batch = packet_batch(packet, authenticated_node)?;
    let mut tx = state.store.observation_pool.begin().await?;
    subject.lock(&mut tx).await?;
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
    let unbound = unbound_attempts(state, &mut tx, batch, retention_enabled).await?;
    let row=sqlx::query("SELECT through_sequence,retained_bytes,event_count FROM observation_nodes WHERE node_id=$1 FOR UPDATE").bind(authenticated_node).fetch_optional(&mut *tx).await?.ok_or(Error::Refused)?;
    transport::require_active_locked(&mut tx, authenticated_node, None).await?;
    if let Some((receipt, refused)) = recorded_receipt(&mut tx, authenticated_node, packet).await? {
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
    let (sources, refused) = batch_sources(&mut tx, batch, &unbound, recorded_at).await?;
    let events = refused.admitted(batch);
    let input = BatchInput {
        sources,
        book: catalog::rate_book(state),
        node: authenticated_node,
        recorded_at,
    };
    let (projections, retained) = aq_observation_ledger::record(&mut tx, &events, input).await?;
    let commit = BatchCommit {
        packet,
        batch,
        events: &events,
        refused,
        projections,
        recorded_at,
        retained,
        retention_enabled,
    };
    let reply = commit.persist(&mut tx, row).await?;
    tx.commit().await?;
    Ok(reply)
}

struct BatchCommit<'a> {
    packet: &'a Packet,
    batch: &'a ObservationBatch,
    events: &'a [&'a ObservationRecord],
    refused: Refusals,
    projections: BatchProjections,
    recorded_at: i64,
    retained: i64,
    retention_enabled: bool,
}

impl BatchCommit<'_> {
    async fn persist(
        self,
        tx: &mut Transaction<'_, Postgres>,
        row: sqlx::postgres::PgRow,
    ) -> Result<BatchReply> {
        let Self {
            packet,
            batch,
            events,
            refused,
            projections,
            recorded_at,
            mut retained,
            retention_enabled,
        } = self;
        let authenticated_node = batch.node_id;
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
            sqlx::query("INSERT INTO observation_source_refusals(node_id,source_id,generation,reason,message_id,sequence,refused_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(authenticated_node).bind(source_id).bind(integer(generation)?).bind(reason).bind(packet.body.message_id).bind(integer(sequence)?).bind(recorded_at).execute(&mut **tx).await?;
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
                    + 4096 * projections.projection_count() as i64,
            )
            .ok_or(Error::Capacity)?;
        let limits = aq_role_settings::observation_limits(tx).await?.0;
        use aq_role_settings::observation::Increase;
        limits
            .admit_events(
                Increase::new(row.get("event_count"), events.len() as i64),
                Increase::new(row.get("retained_bytes"), retained),
            )
            .map_err(Error::Limit)?;
        projections.persist(tx, recorded_at, events).await?;
        sqlx::query("INSERT INTO observation_packets(node_id,message_id,body_digest,receipt,refused) VALUES($1,$2,$3,$4,$5)").bind(authenticated_node).bind(packet.body.message_id).bind(packet.body_digest.as_str()).bind(value(&receipt)?).bind(refused_value).execute(&mut **tx).await?;
        if retention_enabled {
            sqlx::query("INSERT INTO observation_packet_retention(node_id,message_id,first_sequence,last_sequence,attempt_ids,recorded_at_ms,charged_bytes) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(authenticated_node).bind(packet.body.message_id).bind(integer(batch.first_sequence)?).bind(integer(batch.last_sequence)?)
        .bind(projections.current_attempts().collect::<Vec<_>>()).bind(recorded_at).bind(retained).execute(&mut **tx).await?;
        }
        sqlx::query("UPDATE observation_nodes SET through_sequence=$2,retained_bytes=retained_bytes+$3,event_count=event_count+$4 WHERE node_id=$1").bind(authenticated_node).bind(integer(batch.last_sequence)?).bind(retained).bind(events.len() as i64).execute(&mut **tx).await?;
        Ok(BatchReply::new(receipt, refused_sources))
    }
}
