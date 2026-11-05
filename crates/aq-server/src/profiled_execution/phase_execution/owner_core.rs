use crate::{AppState, error::AppError, profiled_execution::on_heap};
use aq_admission::{ArtifactRef, Digest};
use serde::Serialize;
use sqlx::{Postgres, Transaction};
fn refused() -> AppError {
    AppError::Conflict("phase execution authority is not currently available".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn execution_clock(
    time: u64,
    expiry: u64,
    policy_version: u32,
) -> Result<aq_admission::transport::DeliveryClock, AppError> {
    use aq_admission::transport::DeliveryClock;
    if policy_version == 3 {
        DeliveryClock::new_interactive_session(time, expiry)
    } else {
        DeliveryClock::new(time, expiry)
    }
    .map_err(|_| refused())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(refused)
}
fn canonical(v: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut v = serde_json::to_value(v).map_err(|_| AppError::Internal)?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| AppError::Internal)
}
fn digest(v: &impl Serialize) -> Result<Digest, AppError> {
    Ok(Digest::of(&canonical(v)?))
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool, AppError> {
    Ok(canonical(a)? == canonical(b)?)
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='3s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(db)?;
    }
    Ok(tx)
}
/// Same shared retained byte budget as original Accept/Grant. A still-running
/// current-profile owner refuses missing storage rather than undercharging.
pub(super) async fn retained_bytes_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: uuid::Uuid,
) -> Result<(i64, i64), AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let version = version.parse::<u32>().map_err(|_| refused())?;
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if version < 32 && !present {
        return Ok((0, 0));
    }
    if version < 32 || !present {
        return Err(refused());
    }
    sqlx::query_as("SELECT COALESCE(sum(octet_length(request_bytes)::BIGINT+octet_length(receipt_bytes)+octet_length(provenance_bytes)),0)::BIGINT,COALESCE(sum(octet_length(request_bytes)::BIGINT+octet_length(receipt_bytes)+octet_length(provenance_bytes)) FILTER(WHERE node_id=$1),0)::BIGINT FROM profiled_phase_execution_grants").bind(node).fetch_one(&mut **tx).await.map_err(db)
}
async fn require_schema(tx: &mut Transaction<'_, Postgres>) -> Result<(), AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if version.parse::<u32>().map_err(|_| refused())? < 32 {
        return Err(refused());
    }
    let _: (i64, i64) = retained_bytes_locked(tx, uuid::Uuid::nil()).await?;
    Ok(())
}

/// Owning issuer entry point. Transport independently gates the qualified profile.
/// A new execution receipt is issued only from actual current owning facts.
pub async fn prepare(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    request: &phase::PrepareRequest,
    nonce: uuid::Uuid,
) -> Result<phase::ExecutionDelivery, AppError> {
    use aq_admission::transport::TransportDocument;
    TransportDocument::Prepare(request.original.clone())
        .validate()
        .map_err(|_| refused())?;
    request.negotiation.validate().map_err(|_| refused())?;
    if nonce.is_nil()
        || auth.id() != request.original.identity.node_id
        || !matches!(request.original.binding.session,aq_admission::SessionIdentity::Remote{node_id,boot_id,..} if node_id==auth.id()&&boot_id==request.original.identity.boot_id)
    {
        return Err(refused());
    }
    let _request = super::phase_execution::request_permit()?;
    // The initial-owner poll frame must fully return before replay observation
    // work begins. The same request permit covers both bounded branches.
    match Box::pin(prepare_inner(state, auth, request, nonce)).await? {
        PreparationOutcome::Issued(reply) => Ok(*reply),
        PreparationOutcome::Replay(candidate) => {
            Box::pin(finish_replay(state, auth, request, nonce, *candidate)).await
        }
    }
}

enum PreparationOutcome {
    Issued(Box<phase::ExecutionDelivery>),
    Replay(Box<ReplayCandidate>),
}

async fn prepare_inner(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    request: &phase::PrepareRequest,
    nonce: uuid::Uuid,
) -> Result<PreparationOutcome, AppError> {
    use crate::execution::profiled::Status;
    use aq_admission::control::DeliveryStamp;
    use phase::{ExecutionDelivery, ExecutionReceipt};
    use sqlx::Row;
    use uuid::Uuid;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    if let Some(reply) = on_heap(|| replay_locked(state, &mut tx, auth, request, nonce)).await? {
        tx.commit().await.map_err(db)?;
        return Ok(PreparationOutcome::Replay(Box::new(reply)));
    }
    let before = on_heap(|| pinned::load_locked(state, &mut tx, auth, request)).await?;
    tx.commit().await.map_err(db)?;
    let observed = on_heap(|| observations::collect(state, &before.input)).await?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    if let Some(reply) = on_heap(|| replay_locked(state, &mut tx, auth, request, nonce)).await? {
        tx.commit().await.map_err(db)?;
        return Ok(PreparationOutcome::Replay(Box::new(reply)));
    }
    let mut after = on_heap(|| pinned::load_locked(state, &mut tx, auth, request)).await?;
    if !same(&before.history.proposal, &after.history.proposal)?
        || !same(&before.history.request, &after.history.request)?
        || !same(&before.history.receipt, &after.history.receipt)?
        || before.qualification != after.qualification
        || before.registration_id != after.registration_id
        || before.input.source.fact().canonical_bytes()?
            != after.input.source.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    validate_registry_loads(&before, &after)?;
    observed.validate_for(&after.input, now(state)?)?;
    if observed.binding.project.revision != after.input.spec.project_revision
        || observed.binding.node_id != auth.id()
        || !same(
            &observed.binding.project.binding,
            &after.input.decision.project.project.binding,
        )?
    {
        return Err(refused());
    }
    on_heap(|| crate::project_bindings::fence_for_offer(state, &mut tx, &observed.binding)).await?;
    let fence_input = current::Input {
        proposal: &after.history.proposal,
        selection: &after.input.selection,
        decision: &after.input.decision,
        policy: &after.policy,
        material: &after.input.material,
        registry_at_preflight: &before.registry_at_load,
    };
    let current = on_heap(|| current::fence_locked(state, &mut tx, &fence_input)).await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    // A genuinely unissued lineage, not an absent accounting row interpreted as zero.
    let counts =
        on_heap(|| super::no_grant::attempt_counts_locked(&mut tx, after.input.spec.task.task_id))
            .await?;
    if counts.started != 1 || after.hot.grant.is_some() || after.hot.execution_deadline_ms.is_some()
    {
        return Err(refused());
    }
    let issued = now(state)?;
    observed.validate_for(&after.input, issued)?;
    current.fresh_at(issued)?;
    if issued >= after.issuance_ceiling {
        return Err(refused());
    }
    let confirmation = facts::confirmation(
        &after,
        request,
        &observed,
        &current,
        counts.budget_revision(),
        issued,
    )?;
    let deadline = issued
        .checked_add(
            after
                .input
                .offer
                .offered
                .offer
                .manifest
                .resources
                .wall_time_ms
                .min(after.policy.max_lineage_elapsed_ms),
        )
        .filter(|d| *d <= i64::MAX as u64)
        .ok_or_else(refused)?;
    let expiry = issued
        .checked_add(after.history.proposal.policy.maximum_execution_lease_ms)
        .ok_or_else(refused)?
        .min(deadline);
    let grant = aq_session_protocol::ProfiledGrant {
        binding: request.original.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: expiry,
    };
    after.hot.status = Status::Granted;
    after.hot.grant = Some(grant.clone());
    after.hot.execution_deadline_ms = Some(deadline);
    after
        .hot
        .validate_against(&request.original.binding)
        .map_err(|_| refused())?;
    on_heap(|| {
        super::initial_grant::retain_installation_locked(
            &mut tx,
            &after.input.material,
            &request.original.materials[0],
            issued,
        )
    })
    .await?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(request.original.binding.attempt_id)
        .bind(serde_json::to_value(&after.hot).map_err(|_| AppError::Internal)?)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    super::journal::record_prepared(
        &mut tx,
        &request.original.binding,
        after.hot.owner.as_ref().ok_or_else(refused)?,
        grant.grant_id,
        request.original.journal_revision,
        &request.original.journal_digest,
    )
    .await?;
    let row=sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(request.original.binding.attempt_id).fetch_one(&mut *tx).await.map_err(db)?;
    let versions = aq_admission::control::Versions {
        record_revision: row.get::<i64, _>("record_revision") as u64,
        authority_revision: row.get::<i64, _>("authority_revision") as u64,
        grant_revision: row.get::<i64, _>("grant_revision") as u64,
    };
    versions
        .validate_after(&after.versions)
        .map_err(|_| refused())?;
    let receipt = ExecutionReceipt {
        schema_version: 2,
        negotiation: request.negotiation.clone(),
        operation_id: request.original.operation_id,
        request_digest: request.digest().map_err(|_| refused())?,
        accepted_receipt_digest: after.history.receipt.digest().map_err(|_| refused())?,
        issued_at_ms: issued,
        grant,
        execution_deadline_ms: deadline,
        confirmation,
        versions,
    };
    receipt
        .validate_for(
            request,
            &after.history.receipt,
            &after.history.request,
            &after.history.proposal,
        )
        .map_err(|_| refused())?;
    let provenance = make_provenance(&after, &observed, &current);
    let qbytes = canonical(request)?;
    let rbytes = canonical(&receipt)?;
    let pbytes = canonical(&provenance)?;
    if [&qbytes, &rbytes, &pbytes]
        .iter()
        .any(|b| b.is_empty() || b.len() > 131072)
    {
        return Err(refused());
    }
    let incoming = (qbytes.len() + rbytes.len() + pbytes.len()) as u64;
    let quota = on_heap(|| super::preparation::retained_quota_locked(&mut tx, auth.id())).await?;
    if u64::try_from(quota.2)
        .ok()
        .and_then(|b| b.checked_add(incoming))
        .is_none_or(|b| b > 67108864)
        || u64::try_from(quota.3)
            .ok()
            .and_then(|b| b.checked_add(incoming))
            .is_none_or(|b| b > 4194304)
    {
        return Err(refused());
    }
    // Every potentially awaited quota/row acquisition is charged before commit.
    let final_time = now(state)?;
    observed.validate_for(&after.input, final_time)?;
    current.fresh_at(final_time)?;
    if final_time >= after.issuance_ceiling || final_time >= expiry {
        return Err(refused());
    }
    sqlx::query("INSERT INTO profiled_phase_execution_grants(attempt_id,node_id,boot_id,operation_id,grant_id,registration_id,accepted_receipt_digest,request_digest,receipt_digest,request_bytes,receipt_bytes,provenance_bytes,issued_at_ms,expires_at_ms,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)").bind(request.original.binding.attempt_id).bind(auth.id()).bind(request.original.identity.boot_id).bind(request.original.operation_id).bind(receipt.grant.grant_id).bind(provenance.registration_id).bind(receipt.accepted_receipt_digest.as_str()).bind(receipt.request_digest.as_str()).bind(receipt.digest().map_err(|_|refused())?.as_str()).bind(qbytes).bind(rbytes).bind(pbytes).bind(issued as i64).bind(expiry as i64).bind(deadline as i64).execute(&mut *tx).await.map_err(db)?;
    let reply = ExecutionDelivery {
        schema_version: 2,
        receipt,
        delivery: DeliveryStamp {
            nonce,
            clock: execution_clock(
                now(state)?,
                expiry,
                after.history.proposal.policy.schema_version,
            )?,
        },
    };
    reply
        .validate_for(
            request,
            &after.history.receipt,
            &after.history.request,
            &after.history.proposal,
            nonce,
        )
        .map_err(|_| refused())?;
    // INSERT may itself wait on a trigger, FK, or unique index. Charge that
    // final await before making the atomic authority transition durable.
    let final_at = now(state)?;
    observed.validate_for(&after.input, final_at)?;
    current.fresh_at(final_at)?;
    if final_at >= expiry
        || final_at < reply.delivery.clock.server_time_ms
        || final_at >= after.issuance_ceiling
        || reply
            .receipt
            .confirmation
            .facts
            .iter()
            .any(|f| final_at >= f.body.expires_at_ms)
    {
        return Err(refused());
    }
    on_heap(|| {
        crate::delegated_permits::handoff::upgrade_locked(
            state,
            &mut tx,
            auth,
            request,
            &reply.receipt,
        )
    })
    .await?;
    tx.commit().await.map_err(|_| {
        AppError::UpdatesUnavailable(
            "phase execution commit outcome unknown; preserve the exact operation intent".into(),
        )
    })?;
    Ok(PreparationOutcome::Issued(Box::new(reply)))
}

/// Replay conveys an already-issued, still-live exact grant. It neither reruns
/// selection nor makes expired preparation/execution permission fresh again.
struct ReplayCandidate {
    reply: phase::ExecutionDelivery,
    history: history::PhaseExecutionHistory,
    preflight: Option<pinned::Pinned>,
}

async fn replay_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &crate::worker_api::AuthenticatedNode,
    q: &phase::PrepareRequest,
    nonce: uuid::Uuid,
) -> Result<Option<ReplayCandidate>, AppError> {
    use crate::execution::{
        authority::{self, Policy, hot::HotState},
        profiled::Status,
    };
    use aq_admission::control::DeliveryStamp;
    use phase::ExecutionDelivery;
    use sqlx::Row;
    use uuid::Uuid;
    crate::worker_api::lock_current_node(tx, auth).await?;
    let view = on_heap(|| {
        authority::load_locked(state, tx, &q.original.binding.session, Policy::IdentityOnly)
    })
    .await?;
    if view.identity.node_id != auth.id()
        || view.identity.boot_id != q.original.identity.boot_id
        || view.offer_id != q.original.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&q.original.binding.launch_manifest_digest)
        || view.execution_spec_digest != q.original.binding.execution_spec_digest.as_str()
    {
        return Err(refused());
    }
    on_heap(|| super::closure::ensure_launch_open_locked(tx, &q.original.binding)).await?;
    let rows=sqlx::query("SELECT attempt_id,operation_id,grant_id FROM profiled_phase_execution_grants WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) ORDER BY attempt_id LIMIT 2 FOR SHARE").bind(q.original.binding.attempt_id).bind(auth.id()).bind(q.original.operation_id).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.is_empty() {
        let collision:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE node_id=$1 AND operation_id=$2)").bind(auth.id()).bind(q.original.operation_id).fetch_one(&mut **tx).await.map_err(db)?;
        if collision {
            return Err(refused());
        }
        return Ok(None);
    }
    if rows.len() != 1
        || rows[0].get::<Uuid, _>("attempt_id") != q.original.binding.attempt_id
        || rows[0].get::<Uuid, _>("operation_id") != q.original.operation_id
    {
        return Err(refused());
    }
    let original = on_heap(|| {
        history::load_optional_locked(tx, auth.id(), &q.original.binding, rows[0].get("grant_id"))
    })
    .await?
    .ok_or_else(refused)?;
    if !same(&original.request, q)? || !same(&original.installation, &q.original.materials[0])? {
        return Err(refused());
    }
    let runtime = on_heap(|| {
        super::registration::current_runtime_locked(
            state,
            tx,
            auth.id(),
            q.original.identity.boot_id,
        )
    })
    .await?;
    if policy::current(state, &original.accepted, &runtime)? != original.provenance.qualification {
        return Err(refused());
    }
    let current_registration = on_heap(|| {
        super::registration::admitted_runtime_locked(tx, auth.id(), q.original.identity.boot_id)
    })
    .await?;
    let registered =
        on_heap(|| super::closure::history::load_locked(tx, original.provenance.registration_id))
            .await?;
    if current_registration.semantic_digest != registered.metadata.semantic_digest
        || current_registration.request.identity != q.original.identity
    {
        return Err(refused());
    }
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(original.accepted.proposal.spec.lifecycle.project_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| refused())?;
    if project.get::<i64, _>("revision") as u64 != original.accepted.proposal.spec.project_revision
        || !binding.enabled
        || !binding.eligible_node(&state.config.projects, auth.id())
        || !same(&binding, &original.provenance.project.project.binding)?
        || state
            .config
            .profiled_execution
            .as_ref()
            .and_then(|c| {
                c.projects
                    .iter()
                    .find(|p| p.project_id == binding.project_id)
            })
            .is_none_or(|p| !p.enabled)
    {
        return Err(refused());
    }
    let replay_registry = on_heap(|| validate_registry_replay(state, tx, &original)).await?;
    on_heap(|| {
        crate::intelligence::resolver::fence_preparation_input_principal(
            state,
            tx,
            q.original.binding.attempt_id,
        )
    })
    .await?;
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(q.original.binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(raw).map_err(|_| refused())?;
    hot.validate_against(&q.original.binding)
        .map_err(|_| refused())?;
    if !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || hot.status != Status::Granted
        || !same(
            hot.grant.as_ref().ok_or_else(refused)?,
            &original.receipt.grant,
        )?
        || hot.execution_deadline_ms != Some(original.receipt.execution_deadline_ms)
        || hot.owner.as_ref().is_none_or(|o| {
            o.guardian_id != q.original.guardian_id
                || o.launch_id != q.original.launch_id
                || o.reservation_id != q.original.reservation.reservation_id
        })
    {
        return Err(refused());
    }
    let time = now(state)?;
    view.active_at(time)?;
    if time >= original.receipt.execution_deadline_ms {
        return Err(refused());
    }
    let reply = ExecutionDelivery {
        schema_version: 2,
        delivery: DeliveryStamp {
            nonce,
            clock: execution_clock(
                time,
                original.receipt.grant.expires_at_ms,
                original.accepted.proposal.policy.schema_version,
            )?,
        },
        receipt: original.receipt.clone(),
    };
    reply
        .validate_for(
            q,
            &original.accepted.receipt,
            &original.accepted.request,
            &original.accepted.proposal,
            nonce,
        )
        .map_err(|_| refused())?;
    on_heap(|| {
        crate::delegated_permits::handoff::upgrade_locked(state, tx, auth, q, &original.receipt)
    })
    .await?;
    let preflight =
        on_heap(|| replay_preflight(state, tx, auth, q, &original, &replay_registry)).await?;
    Ok(Some(ReplayCandidate {
        reply,
        history: original,
        preflight,
    }))
}

/// Fresh replay observations authorize delivery of the original receipt only.
/// They are not serialized as replacement issue facts and create no new grant,
/// budget/accounting origin, journal transition or private retrieval permission.
async fn finish_replay(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    request: &phase::PrepareRequest,
    nonce: uuid::Uuid,
    original: ReplayCandidate,
) -> Result<phase::ExecutionDelivery, AppError> {
    let Some(before) = original.preflight else {
        // Frozen019 deliberately keeps its original exact replay algorithm.
        return Ok(original.reply);
    };
    let observed = observations::collect(state, &before.input).await?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let current_replay = replay_locked(state, &mut tx, auth, request, nonce)
        .await?
        .ok_or_else(refused)?;
    let after = current_replay.preflight.as_ref().ok_or_else(refused)?;
    if !same(&original.history.receipt, &current_replay.history.receipt)?
        || !same(
            &original.history.provenance,
            &current_replay.history.provenance,
        )?
        || !same(&original.history.request, &current_replay.history.request)?
        || original.history.receipt_digest != current_replay.history.receipt_digest
        || !same(&before.history.proposal, &after.history.proposal)?
        || !same(&before.history.request, &after.history.request)?
        || !same(&before.history.receipt, &after.history.receipt)?
        || before.qualification != after.qualification
        || before.registration_id != after.registration_id
        || before.input.source.fact().canonical_bytes()?
            != after.input.source.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    validate_registry_loads(&before, after)?;
    observed.validate_for(&after.input, now(state)?)?;
    if observed.binding.project.revision != after.input.spec.project_revision
        || observed.binding.node_id != auth.id()
        || !same(
            &observed.binding.project.binding,
            &after.input.decision.project.project.binding,
        )?
    {
        return Err(refused());
    }
    crate::project_bindings::fence_for_offer(state, &mut tx, &observed.binding).await?;
    let current = current::fence_locked(
        state,
        &mut tx,
        &current::Input {
            proposal: &after.history.proposal,
            selection: &after.input.selection,
            decision: &after.input.decision,
            policy: &after.policy,
            material: &after.input.material,
            registry_at_preflight: &before.registry_at_load,
        },
    )
    .await?;
    // This is after every I/O and lock wait. All relevant authority rows remain
    // locked through commit; source ages and the *original* grant still advance.
    let final_at = now(state)?;
    observed.validate_for(&after.input, final_at)?;
    current.fresh_at(final_at)?;
    if final_at < original.history.receipt.issued_at_ms
        || final_at >= original.history.receipt.grant.expires_at_ms
        || final_at >= original.history.receipt.execution_deadline_ms
        || final_at >= after.issuance_ceiling
    {
        return Err(refused());
    }
    tx.commit().await.map_err(db)?;
    // Commit is the final awaited boundary. Even a delayed read-only commit
    // cannot deliver aged observations or revive an expired original grant.
    let delivery_at = now(state)?;
    observed.validate_for(&after.input, delivery_at)?;
    current.fresh_at(delivery_at)?;
    if delivery_at < final_at || delivery_at >= after.issuance_ceiling {
        return Err(refused());
    }
    let mut reply = original.reply;
    reply.delivery.clock = execution_clock(
        delivery_at,
        reply.receipt.grant.expires_at_ms,
        after.history.proposal.policy.schema_version,
    )?;
    reply
        .validate_for(
            request,
            &after.history.receipt,
            &after.history.request,
            &after.history.proposal,
            nonce,
        )
        .map_err(|_| refused())?;
    Ok(reply)
}
