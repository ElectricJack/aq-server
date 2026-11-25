/// Replay conveys an already-issued, still-live exact grant. It neither reruns
/// selection nor makes expired preparation/execution permission fresh again.
struct ReplayCandidate {
    reply: phase::ExecutionDelivery,
    history: history::PhaseExecutionHistory,
    preflight: Option<pinned::Pinned>,
}

async fn replay_locked(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &PhasePreparation<'_>,
) -> Result<Option<ReplayCandidate>, AppError> {
    let PhasePreparation {
        state,
        auth,
        request: q,
        nonce,
    } = *preparation;
    use aq_admission::control::DeliveryStamp;
    use phase::ExecutionDelivery;
    let view = lock_replay_authority(tx, preparation).await?;
    let Some(original) = find_replay_grant(tx, preparation).await? else {
        return Ok(None);
    };
    verify_registered_replay(tx, preparation, &original).await?;
    fence_replay_project(tx, preparation, &original).await?;
    let replay_registry = on_heap(|| validate_registry_replay(state, tx, &original)).await?;
    on_heap(|| {
        crate::intelligence::resolver::fence_preparation_input_principal(
            state,
            tx,
            q.original.binding.attempt_id,
        )
    })
    .await?;
    fence_replay_hot_state(tx, q, &view, &original).await?;
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
    preparation: &PhasePreparation<'_>,
    original: ReplayCandidate,
) -> Result<phase::ExecutionDelivery, AppError> {
    let PhasePreparation {
        state,
        request,
        nonce,
        ..
    } = *preparation;
    let Some(before) = original.preflight else {
        // Frozen019 deliberately keeps its original exact replay algorithm.
        return Ok(original.reply);
    };
    let observed = observations::collect(state, &before.input).await?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let current_replay = replay_locked(&mut tx, preparation)
        .await?
        .ok_or_else(|| refused())?;
    let after = current_replay.preflight.as_ref().ok_or_else(|| refused())?;
    verify_replayed_receipt(&original.history, &current_replay.history)?;
    check_preflight(preparation, &before, after, &observed)?;
    crate::project_bindings::fence_for_offer(state, &mut tx, &observed.binding).await?;
    let current =
        current::fence_locked(state, &mut tx, &current_fence_input(&before, after)).await?;
    // This is after every I/O and lock wait. All relevant authority rows remain
    // locked through commit; source ages and the *original* grant still advance.
    let final_at = now(state)?;
    observed.validate_for(&after.input, final_at)?;
    current.fresh_at(final_at)?;
    let receipt = &original.history.receipt;
    let replay_ceiling = receipt
        .grant
        .expires_at_ms
        .min(receipt.execution_deadline_ms)
        .min(after.issuance_ceiling);
    if !(receipt.issued_at_ms..replay_ceiling).contains(&final_at) {
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
