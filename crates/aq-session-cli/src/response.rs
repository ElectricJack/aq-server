//! Bind typed mutation acknowledgements to the originating operation.
use super::*;
use aq_session_protocol::TerminalIdentity;

pub(super) fn bound(request: &Mutation, reply: &MutationReply) -> bool {
    let Some((target, operation)) = reply_identity(reply) else {
        return false;
    };
    if (target, operation) != expected_identity(request) {
        return false;
    }
    match (request, reply) {
        (
            Mutation::Watch(r),
            MutationReply::Watch {
                subscription_id,
                session_revision,
                expires_at_ms,
                ..
            },
        ) => {
            (subscription_id, session_revision) == (&r.operation_id, &r.expected_session_revision)
                && *expires_at_ms <= r.expires_at_ms
        }
        (Mutation::Control(r), MutationReply::Control { status, .. }) => {
            status.session_revision == r.expected_session_revision
        }
        (Mutation::Renew(r), MutationReply::Control { status, .. }) => {
            status.session_revision == r.expected_session_revision
        }
        (Mutation::Release(r), MutationReply::Released { generation, .. }) => {
            Some(*generation) == r.expected_controller_generation.checked_add(1)
        }
        (
            Mutation::Input(_) | Mutation::EpochInput(_) | Mutation::Resolve(_),
            MutationReply::Input(_),
        ) => true,
        (Mutation::Resize(r), MutationReply::Resize { sequence, .. }) => *sequence == r.sequence,
        (
            Mutation::Capture(r),
            MutationReply::Capture {
                capture_request_id,
                expires_at_ms,
                ..
            },
        ) => !capture_request_id.is_nil() && *expires_at_ms <= r.expires_at_ms,
        (Mutation::OpenEpoch(r), MutationReply::Epoch { permit, .. }) => {
            permit.validate().is_ok()
                && (permit.lease_id, permit.controller_generation)
                    == (r.lease_id, r.expected_controller_generation)
        }
        _ => false,
    }
}

fn reply_identity(reply: &MutationReply) -> Option<(&TerminalIdentity, Uuid)> {
    match reply {
        MutationReply::Watch {
            terminal,
            operation_id,
            ..
        }
        | MutationReply::Released {
            terminal,
            operation_id,
            ..
        }
        | MutationReply::Resize {
            terminal,
            operation_id,
            ..
        }
        | MutationReply::Capture {
            terminal,
            operation_id,
            ..
        } => Some((terminal, *operation_id)),
        MutationReply::Control {
            status,
            operation_id,
        } => Some((&status.terminal, *operation_id)),
        MutationReply::Input(r) => Some((&r.terminal, r.operation_id)),
        MutationReply::Epoch {
            operation_id,
            permit,
        } => Some((&permit.terminal, *operation_id)),
        MutationReply::Registered { .. } | MutationReply::Access { .. } => None,
    }
}

pub(super) fn expected_identity(request: &Mutation) -> (&TerminalIdentity, Uuid) {
    match request {
        Mutation::Watch(r) => (&r.terminal, r.operation_id),
        Mutation::Control(r) => (&r.terminal, r.operation_id),
        Mutation::Renew(r) => (&r.terminal, r.operation_id),
        Mutation::Release(r) => (&r.terminal, r.operation_id),
        Mutation::Input(r) => (&r.terminal, r.operation_id),
        Mutation::EpochInput(r) => (&r.input.terminal, r.input.operation_id),
        Mutation::OpenEpoch(r) => (&r.terminal, r.operation_id),
        Mutation::Resize(r) => (&r.terminal, r.operation_id),
        Mutation::Capture(r) => (&r.terminal, r.operation_id),
        Mutation::Resolve(r) => (&r.terminal, r.uncertain_operation_id),
    }
}

pub(super) fn check_state(reply: &MutationReply, exact: &TerminalIdentity) -> Result<(), String> {
    match reply {
        MutationReply::Control { status, .. } => {
            if let Some(lease) = &status.lease {
                lease.validate().map_err(|_| "invalid controller reply")?;
                if (&lease.terminal, lease.generation) != (exact, status.generation) {
                    return Err("controller reply binding mismatch".into());
                }
            }
        }
        MutationReply::Input(receipt)
            if receipt.disposition == InputDisposition::DeliveryUnknown
                && !receipt.input_paused =>
        {
            return Err("invalid unknown receipt".into());
        }
        _ => {}
    }
    Ok(())
}
