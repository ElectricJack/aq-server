async fn prepare_inner(preparation: &PhasePreparation<'_>) -> Result<PreparationOutcome, AppError> {
    let PhasePreparation {
        state,
        auth,
        request,
        nonce,
    } = *preparation;
    use crate::execution::profiled::Status;
    use aq_admission::control::DeliveryStamp;
    use phase::{ExecutionDelivery, ExecutionReceipt};
    use sqlx::Row;
    use uuid::Uuid;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    if let Some(reply) = on_heap(|| replay_locked(&mut tx, preparation)).await? {
        tx.commit().await.map_err(db)?;
        return Ok(PreparationOutcome::Replay(Box::new(reply)));
    }
    let before = on_heap(|| pinned::load_locked(state, &mut tx, auth, request)).await?;
    tx.commit().await.map_err(db)?;
    let observed = on_heap(|| observations::collect(state, &before.input)).await?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    if let Some(reply) = on_heap(|| replay_locked(&mut tx, preparation)).await? {
        tx.commit().await.map_err(db)?;
        return Ok(PreparationOutcome::Replay(Box::new(reply)));
    }
    let mut after = on_heap(|| pinned::load_locked(state, &mut tx, auth, request)).await?;
    check_preflight(preparation, &before, &after, &observed)?;
    on_heap(|| crate::project_bindings::fence_for_offer(state, &mut tx, &observed.binding)).await?;
    let fence_input = current_fence_input(&before, &after);
    let current = on_heap(|| current::fence_locked(state, &mut tx, &fence_input)).await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    // A genuinely unissued attempt, not an absent accounting row interpreted as
    // zero: the lineage holds exactly the decision's started attempts plus this
    // one (1 for a first attempt; a B4 replacement counts its predecessors).
    let counts =
        on_heap(|| super::no_grant::attempt_counts_locked(&mut tx, after.input.spec.task.task_id))
            .await?;
    require_unissued_lineage(&after, counts.started)?;
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
                .wall_time_ms,
        )
        .filter(|d| *d <= i64::MAX as u64)
        .ok_or_else(|| refused())?;
    let expiry = issued
        .checked_add(after.history.proposal.policy.maximum_execution_lease_ms)
        .ok_or_else(|| refused())?
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
        after.hot.owner.as_ref().ok_or_else(|| refused())?,
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
    verify_commit_deadline(&after, &reply, final_at)?;
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

fn require_unissued_lineage(after: &pinned::Pinned, started: u32) -> Result<(), AppError> {
    let expected = after
        .input
        .decision
        .inputs
        .budget
        .attempts_started
        .checked_add(1)
        .ok_or_else(|| refused())?;
    if started != expected
        || (after.input.decision.inputs.replacement.is_none() && expected != 1)
        || after.hot.grant.is_some()
        || after.hot.execution_deadline_ms.is_some()
    {
        return Err(refused());
    }
    Ok(())
}

fn verify_commit_deadline(
    after: &pinned::Pinned,
    reply: &phase::ExecutionDelivery,
    final_at: u64,
) -> Result<(), AppError> {
    if final_at >= reply.receipt.grant.expires_at_ms
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
    Ok(())
}
