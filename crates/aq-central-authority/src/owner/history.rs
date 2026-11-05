use super::*;

/// The caller holds the head lock. Never trust a caller-supplied fence to erase
/// old pending/unknown/handed-off input or reuse an operation in another role.
pub(super) async fn fence(
    tx: &mut Transaction<'_, Postgres>,
    h: &Head,
    plan: &CentralPlan,
) -> OwnerResult<MailHistoryFence> {
    let rows = sqlx::query("SELECT * FROM central_session_generations WHERE actor_id=$1 ORDER BY generation LIMIT 1025")
        .bind(h.actor).fetch_all(&mut **tx).await?;
    if rows.len() != h.generation as usize {
        return Err(OwnerError::Storage);
    }
    let mut states = Vec::with_capacity(rows.len());
    let mut pins = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let s = snapshot(row, h)?;
        if s.run.plan().generation != i as u64 + 1 {
            return Err(OwnerError::Storage);
        }
        pins.push((
            s.run.plan().generation,
            s.revision,
            Digest::of(state_bytes(&s.run)?.as_bytes()),
        ));
        states.push(s.run);
    }
    let mut entries = Vec::with_capacity(plan.batch.messages.len());
    for message in &plan.batch.messages {
        let mut disposition = HistoryDisposition::NoPriorDispatch;
        for run in &states {
            for previous in &run.plan().batch.messages {
                if previous.message_id != message.message_id && previous.cursor != message.cursor {
                    continue;
                }
                if previous != message {
                    return Err(OwnerError::Conflict);
                }
                let next = match run.dispatch() {
                    DispatchState::NotAttempted => continue,
                    DispatchState::IntentCommitted => HistoryDisposition::Held {
                        association: run.association().ok_or(OwnerError::Storage)?.clone(),
                        reason: MailHoldReason::Pending,
                    },
                    DispatchState::Unknown { .. } => HistoryDisposition::Held {
                        association: run.association().ok_or(OwnerError::Storage)?.clone(),
                        reason: MailHoldReason::Unknown,
                    },
                    DispatchState::Resolved { evidence }
                        if evidence.disposition == HandoffDisposition::NotHandedOff =>
                    {
                        HistoryDisposition::Retryable {
                            evidence: *evidence.clone(),
                        }
                    }
                    DispatchState::Resolved { evidence } => HistoryDisposition::Held {
                        association: evidence.association.clone(),
                        reason: MailHoldReason::HandedOff,
                    },
                };
                if !matches!(disposition, HistoryDisposition::Held { .. }) {
                    disposition = next;
                }
            }
        }
        entries.push(MessageHistory {
            message: message.clone(),
            disposition,
        });
    }
    let mut operations = Vec::with_capacity(2);
    for operation_id in [plan.dispatch_id, plan.host_operation_id] {
        // Even an operation reserved before physical start is not reusable.
        if states
            .iter()
            .any(|s| [s.plan().dispatch_id, s.plan().host_operation_id].contains(&operation_id))
        {
            return Err(OwnerError::Conflict);
        }
        operations.push(OperationHistory {
            operation_id,
            disposition: OperationDisposition::Unused,
        });
    }
    let bytes = serde_json::to_vec(&pins).map_err(|_| OwnerError::Storage)?;
    Ok(MailHistoryFence {
        schema_version: 1,
        actor_id: h.actor,
        conversation_id: h.conversation,
        through_generation: h.generation,
        source_snapshot: ArtifactRef {
            id: Text::new("aq.central-session-history").map_err(|_| OwnerError::Storage)?,
            version: Text::new("1").map_err(|_| OwnerError::Storage)?,
            digest: Digest::of(&bytes),
        },
        entries,
        operations,
    })
}
