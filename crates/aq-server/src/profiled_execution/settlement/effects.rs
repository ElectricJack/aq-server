//! Exhaustive bounded reads of the existing AQ-07 attempt-owned ledger.
//! New commands take common authority before binding/operation locks. Dispatch
//! completion and cancellation take only operation locks; we never acquire a
//! node/attempt after taking an operation. No SQL connection crosses remote I/O.
use crate::{AppState, error::AppError, profiled_execution::results};
use aq_admission::{ArtifactRef, Digest};
pub use aq_profiled_state::effect_profile::LedgerDisposition;
use aq_profiled_state::effect_profile::verdict;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub const MAX_OPERATIONS: usize = 4096;
pub const MAX_INSPECTION_BYTES: u64 = 32 * 1024 * 1024;

/// An observation of actual retained owners, not a quiescence/release receipt.
/// Private fields and no Deserialize prevent a request from supplying a verdict.
/// A future mutation owner must repeat this read in its own final transaction.
pub struct AqEffectsSnapshot {
    report: ArtifactRef,
    stopped: ArtifactRef,
    profile: Digest,
    ledger: Digest,
    count: usize,
    bytes: u64,
    disposition: LedgerDisposition,
    terminal: bool,
}
impl AqEffectsSnapshot {
    #[cfg(test)]
    pub(super) fn synthetic(
        result: &results::RecordedResult,
        disposition: LedgerDisposition,
    ) -> Self {
        Self {
            report: result.receipt_artifact.clone(),
            stopped: result.receipt.stopped_receipt.clone(),
            profile: result.spec.graph_access_profile.digest.clone(),
            ledger: Digest::of(b"synthetic owner fixture; no actual ledger"),
            count: 0,
            bytes: 0,
            terminal: disposition != LedgerDisposition::NeedsReconciliation,
            disposition,
        }
    }
    pub fn digest(&self) -> &Digest {
        &self.ledger
    }
    pub fn operation_count(&self) -> usize {
        self.count
    }
    pub fn inspected_bytes(&self) -> u64 {
        self.bytes
    }
    /// The acceptance verdict: only a read-only, mail-free ledger is `Settled`.
    pub fn disposition(&self) -> LedgerDisposition {
        self.disposition
    }
    /// B4 quiescence, not an acceptance verdict: every Graph operation of the attempt is final
    /// at its dispatch owner, so nothing it started can still land. Mail send and ack commit
    /// with their mailbox operation and are never in flight, and a validated profile names no
    /// capability outside these two ledgers. A successor never re-dispatches these operations
    /// (each keeps its own idempotency key), so replacing the attempt neither loses nor repeats
    /// an effect. Messaging or write capabilities still refuse acceptance (`disposition`).
    pub fn quiescent(&self) -> bool {
        self.terminal
    }
    pub(super) fn matches(&self, result: &results::RecordedResult) -> bool {
        self.report == result.receipt_artifact
            && self.stopped == result.receipt.stopped_receipt
            && self.profile == result.spec.graph_access_profile.digest
    }
}

#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::UpdatesUnavailable("complete stopped-attempt effect ledger unavailable".into())
}
fn canonical(value: &Value) -> Result<Vec<u8>, AppError> {
    aq_agent_api::canonical(value).map_err(|_| refused())
}
fn digest(value: &Value) -> Result<String, AppError> {
    aq_agent_api::digest(value).map_err(|_| refused())
}
fn byte_budget(count: usize, sizes: impl IntoIterator<Item = i64>) -> Result<u64, AppError> {
    if count > MAX_OPERATIONS {
        return Err(refused());
    }
    sizes.into_iter().try_fold(0u64, |sum, size| {
        let n = u64::try_from(size).map_err(|_| refused())?;
        sum.checked_add(n)
            .filter(|n| *n <= MAX_INSPECTION_BYTES)
            .ok_or_else(|| refused())
    })
}

fn validate_launch_binding(
    current: &results::RecordedResult,
    binding: &Value,
    profile: &Value,
) -> Result<Uuid, AppError> {
    let attempt = current.report.binding.attempt_id;
    let expected = &current.report.binding;
    let aq_admission::SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        ..
    } = expected.session
    else {
        return Err(refused());
    };
    if !aq_agent_api::valid("Binding", binding)
        || !aq_agent_api::valid("ResolvedProfile", profile)
        || binding["attempt_id"] != json!(attempt)
        || binding["session_id"] != json!(session_id)
        || binding["generation"] != json!(expected.assignment_generation)
        || binding["node_id"] != json!(node_id)
        || binding["boot_id"] != json!(boot_id)
        || binding["task_id"] != json!(current.spec.task.task_id)
        || binding["workspace_id"] != json!(current.spec.task.workspace_id)
        || binding["project_id"] != json!(current.spec.lifecycle.project_id)
        || binding["resolved_spec_digest"] != json!(expected.execution_spec_digest.as_str())
        || binding["profile_digest"] != digest(profile)?
        || digest(profile)? != current.spec.graph_access_profile.digest.as_str()
    {
        return Err(refused());
    }
    Ok(node_id)
}

fn operation_bytes(
    meta: &sqlx::postgres::PgRow,
    row: &Value,
    attempt: Uuid,
    node: Uuid,
) -> Result<Vec<u8>, AppError> {
    let operation: Uuid = meta.get("operation_id");
    if row["attempt_id"] != json!(attempt)
        || row["node_id"] != json!(node)
        || row["operation_id"] != json!(operation)
        || row["operation_seq"] != json!(meta.get::<i64, _>("operation_seq"))
    {
        return Err(refused());
    }
    let encoded = canonical(row)?;
    // Recheck actual serialized bytes too; metadata's fixed margin includes
    // finite scalar fields. Refusal never truncates or silently skips a row.
    if encoded.len() as u64 > meta.get::<i64, _>("inspected_bytes") as u64 {
        return Err(refused());
    }
    Ok(encoded)
}

/// Load the actual B1/stop owners FIRST, then sealed agent binding, then bounded
/// operation rows. Neither the caller's pending_effect_ids nor an empty supplied
/// list establishes completeness. The returned digest includes every row.
pub async fn inspect_aq_effects_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    recorded: &results::RecordedResult,
) -> Result<AqEffectsSnapshot, AppError> {
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut **tx)
        .await
        .map_err(|_| refused())?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut **tx)
        .await
        .map_err(|_| refused())?;
    let current = results::load_locked(state, tx, recorded.report.report_id).await?;
    if !current.is_current_report_head
        || current.head_report_id != current.report.report_id
        || current.receipt_artifact != recorded.receipt_artifact
        || current.report.binding != recorded.report.binding
    {
        return Err(refused());
    }
    let attempt = current.report.binding.attempt_id;
    // B1's owner load has acquired common node/attempt/hot locks and verified
    // actual stopped history. A separate GraphActive check could not succeed
    // for this stopped attempt and would improperly prevent recovery reads.
    let launch = sqlx::query(
        "SELECT binding,profile FROM agent_launch_bindings WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(attempt)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| refused())?
    .ok_or_else(|| refused())?;
    let binding: Value = launch.get("binding");
    let profile: Value = launch.get("profile");
    let node_id = validate_launch_binding(&current, &binding, &profile)?;
    // Lock metadata first: JSON/TOAST payloads are not fetched until total bytes
    // have been checked. Existing attempt admission and these row locks keep the
    // subsequent bounded payload read on this exact stable retained set.
    let metadata = sqlx::query("SELECT operation_id,operation_seq,octet_length(request::text)::bigint+octet_length(mapped::text)+octet_length(policy::text)+octet_length(status::text)+2048 AS inspected_bytes FROM agent_graph_operations WHERE attempt_id=$1 ORDER BY operation_seq LIMIT 4097 FOR SHARE")
        .bind(attempt).fetch_all(&mut **tx).await.map_err(|_| refused())?;
    let bytes = byte_budget(
        metadata.len(),
        metadata.iter().map(|r| r.get("inspected_bytes")),
    )?;
    let sender_key = digest(&json!({"kind":"session", "binding":binding}))?;
    // Exact owner sender namespace has a leading-key PK. An absent row is an
    // actual indexed owner read, not an omitted agent declaration. Atomic mail
    // send/ack always inserts this operation alongside its effect.
    let mailed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_operations WHERE sender_key=$1)")
            .bind(sender_key)
            .fetch_one(&mut **tx)
            .await
            .map_err(|_| refused())?;
    let mut terminal = true;
    let mut hasher = Sha256::new();
    hasher.update(canonical(&json!({"domain":"aq10-aq-effects-observation-v1","report":current.receipt_artifact,"stopped":current.receipt.stopped_receipt,"profile":current.spec.graph_access_profile,"binding":binding,"count":metadata.len(),"inspected_bytes":bytes,"mailbox_operation_exists":mailed}))?);
    let rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(o) FROM agent_graph_operations o WHERE attempt_id=$1 ORDER BY operation_seq LIMIT 4097")
        .bind(attempt).fetch_all(&mut **tx).await.map_err(|_| refused())?;
    if rows.len() != metadata.len() {
        return Err(refused());
    }
    for (meta, row) in metadata.iter().zip(&rows) {
        let encoded = operation_bytes(meta, row, attempt, node_id)?;
        hasher.update((encoded.len() as u64).to_be_bytes());
        hasher.update(&encoded);
        terminal &=
            aq_agent_api::operation_receipt::settled(row, &binding).map_err(|_| refused())?;
    }
    let disposition = verdict(mailed, &profile, terminal);
    Ok(AqEffectsSnapshot {
        report: current.receipt_artifact,
        stopped: current.receipt.stopped_receipt,
        profile: current.spec.graph_access_profile.digest,
        ledger: Digest::of(&hasher.finalize()),
        count: metadata.len(),
        bytes,
        disposition,
        terminal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_and_cardinality_limits_refuse_instead_of_truncating() {
        assert_eq!(
            byte_budget(MAX_OPERATIONS, [MAX_INSPECTION_BYTES as i64]).unwrap(),
            MAX_INSPECTION_BYTES
        );
        assert!(byte_budget(MAX_OPERATIONS + 1, [0]).is_err());
        assert!(byte_budget(1, [MAX_INSPECTION_BYTES as i64, 1]).is_err());
        assert!(byte_budget(1, [-1]).is_err());
        assert!(byte_budget(2, [i64::MAX, i64::MAX]).is_err());
    }
    /// Acceptance stays the read-only pilot; B4 quiescence is only "every operation is final"
    /// (2026-11-21: a developer profile with `message.send` and `write.*` could never be
    /// replaced, so its finished attempt held its task with `replacement_effects_unreconciled`).
    #[test]
    fn messaging_and_write_ledgers_refuse_acceptance_but_are_quiescent_when_final() {
        let developer = json!({"capabilities":["read.get","write.finding","message.send"]});
        let writer = json!({"capabilities":["read.get","write.task_content"]});
        let reader = json!({"capabilities":["read.get"]});
        use LedgerDisposition::*;
        for (mailed, profile, terminal, expected) in [
            (false, &developer, true, UnsupportedMessaging),
            (true, &reader, true, UnsupportedMessaging),
            (false, &writer, true, UnsupportedProfile),
            (false, &reader, true, Settled),
            (false, &reader, false, NeedsReconciliation),
            (false, &developer, false, UnsupportedMessaging),
        ] {
            let d = verdict(mailed, profile, terminal);
            assert_eq!(d, expected, "{profile} mailed={mailed} terminal={terminal}");
            let snapshot = AqEffectsSnapshot {
                terminal,
                ..snapshot(d)
            };
            assert_eq!(snapshot.quiescent(), terminal, "{}", d.name());
            assert_eq!(snapshot.disposition() == Settled, d == Settled);
        }
    }
    fn snapshot(disposition: LedgerDisposition) -> AqEffectsSnapshot {
        let r = |id: &str| ArtifactRef {
            id: aq_admission::Text::new(id).unwrap(),
            version: aq_admission::Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        };
        AqEffectsSnapshot {
            report: r("report"),
            stopped: r("stopped"),
            profile: Digest::of(b"profile"),
            ledger: Digest::of(b"ledger"),
            count: 1,
            bytes: 1,
            disposition,
            terminal: true,
        }
    }
}
