//! Bounded historical/current reads. No reconciliation input is adopted as
//! authority, a journal head, a stop proof or permission to release a claim.
mod api;
mod receipts;
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::hot::HotState,
        profiled::{Status, Stop},
    },
    worker_api::AuthenticatedNode,
};
pub use api::routes;
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, SessionIdentity,
    control::{
        CurrentAuthority, CurrentPhase, FailureCode, LaunchOwner, ReconcileEntry, ReconcilePage,
        Versions,
    },
    transport::{PreparationPlan, Reconcile, ReconcileAttempt, TransportDocument},
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
fn invalid() -> AppError {
    AppError::Conflict("native reconciliation history is unavailable or inconsistent".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::UpdatesUnavailable("native reconciliation read unavailable".into())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(invalid)
}
fn owner(hot: &HotState) -> Option<LaunchOwner> {
    hot.owner.as_ref().map(|o| LaunchOwner {
        reservation_id: o.reservation_id,
        guardian_id: o.guardian_id,
        launch_id: o.launch_id,
    })
}
struct Snapshot {
    binding: LaunchBinding,
    versions: Versions,
    hot: HotState,
    holds_task: bool,
    offer_expiry: u64,
    preparation: Option<(PreparationPlan, Digest)>,
    started: Option<Uuid>,
    terminal: Option<ArtifactRef>,
}
impl Snapshot {
    fn stop(&self, reason: FailureCode) -> CurrentPhase {
        CurrentPhase::StopRequired {
            owner: owner(&self.hot),
            last_grant_id: self.hot.grant.as_ref().map(|g| g.grant_id),
            execution_deadline_ms: self.hot.execution_deadline_ms,
            reason,
        }
    }
    fn render(self, now: u64, current_boot: bool) -> Result<CurrentAuthority, AppError> {
        let phase = if self.hot.status == Status::NeverGranted {
            CurrentPhase::NeverGranted {
                closure: self.terminal.clone().ok_or_else(invalid)?,
            }
        } else if !self.holds_task {
            // No qualified public release artifact exists for this first profile.
            return Err(invalid());
        } else if matches!(self.hot.stop, Stop::Stopped { .. }) {
            CurrentPhase::Stopped {
                owner: owner(&self.hot).ok_or_else(invalid)?,
                grant_id: self.hot.grant.as_ref().ok_or_else(invalid)?.grant_id,
                stopped_receipt: self.terminal.clone().ok_or_else(invalid)?,
            }
        } else if self.hot.revoked {
            self.stop(FailureCode::CredentialRevoked)
        } else if self.hot.cancellation_id.is_some() {
            self.stop(FailureCode::Cancelled)
        } else if !current_boot
            || matches!(
                self.hot.status,
                Status::Unknown | Status::Cancelling | Status::Finished | Status::Declined
            )
        {
            self.stop(FailureCode::RecoveryRequired)
        } else {
            match self.hot.status {
                Status::Offered if self.offer_expiry > now => CurrentPhase::Offered {
                    expires_at_ms: self.offer_expiry,
                },
                Status::Preparing => {
                    let (plan, digest) = self.preparation.as_ref().ok_or_else(invalid)?;
                    if plan.clock.expires_at_ms <= now {
                        self.stop(FailureCode::Expired)
                    } else {
                        CurrentPhase::Preparing {
                            owner: owner(&self.hot).ok_or_else(invalid)?,
                            preparation_operation_id: plan.operation_id,
                            preparation_digest: digest.clone(),
                            expires_at_ms: plan.clock.expires_at_ms,
                        }
                    }
                }
                Status::Granted => {
                    let grant = self.hot.grant.as_ref().ok_or_else(invalid)?;
                    if grant.expires_at_ms <= now {
                        self.stop(FailureCode::Expired)
                    } else {
                        CurrentPhase::Granted {
                            owner: owner(&self.hot).ok_or_else(invalid)?,
                            grant_id: grant.grant_id,
                            expires_at_ms: grant.expires_at_ms,
                            execution_deadline_ms: self
                                .hot
                                .execution_deadline_ms
                                .ok_or_else(invalid)?,
                            started_operation_id: self.started,
                        }
                    }
                }
                _ => self.stop(FailureCode::Expired),
            }
        };
        let authority = CurrentAuthority {
            binding: self.binding,
            versions: self.versions,
            phase,
        };
        authority.validate(now).map_err(|_| invalid())?;
        Ok(authority)
    }
}
fn scope(binding: &LaunchBinding, node: Uuid, boot: Uuid) -> bool {
    matches!(binding.session,SessionIdentity::Remote {node_id,boot_id,..} if node_id==node && boot_id==boot)
}
async fn snapshot(
    tx: &mut Transaction<'_, Postgres>,
    row: sqlx::postgres::PgRow,
    request: &Reconcile,
    expected: Option<&ReconcileAttempt>,
) -> Result<Snapshot, AppError> {
    let binding: LaunchBinding =
        serde_json::from_value(row.get("launch_binding")).map_err(|_| invalid())?;
    aq_session_protocol::Validate::validate(&binding).map_err(|_| invalid())?;
    if !scope(&binding, request.identity.node_id, request.identity.boot_id)
        || binding.attempt_id != row.get::<Uuid, _>("attempt_id")
        || binding.offer_id != row.get::<Uuid, _>("offer_id")
        || binding.execution_spec_digest.as_str() != row.get::<String, _>("execution_spec_digest")
        || binding.launch_manifest_digest.as_str() != row.get::<String, _>("manifest_digest")
    {
        return Err(invalid());
    }
    let SessionIdentity::Remote { session_id, .. } = binding.session else {
        return Err(invalid());
    };
    if session_id != row.get::<Uuid, _>("session_id") {
        return Err(invalid());
    }
    let hot: HotState = serde_json::from_value(row.get("hot")).map_err(|_| invalid())?;
    hot.validate_against(&binding).map_err(|_| invalid())?;
    if let Some(expected) = expected
        && (expected.binding != binding
            || hot.owner.as_ref().is_some_and(|o| {
                o.guardian_id != expected.guardian_id || o.launch_id != expected.launch_id
            })
            || expected
                .last_grant_id
                .is_some_and(|id| hot.grant.as_ref().is_none_or(|g| g.grant_id != id)))
    {
        return Err(invalid());
    }
    let versions = Versions {
        record_revision: row
            .get::<i64, _>("record_revision")
            .try_into()
            .map_err(|_| invalid())?,
        authority_revision: row
            .get::<i64, _>("authority_revision")
            .try_into()
            .map_err(|_| invalid())?,
        grant_revision: row
            .get::<i64, _>("grant_revision")
            .try_into()
            .map_err(|_| invalid())?,
    };
    versions.validate().map_err(|_| invalid())?;
    if (versions.grant_revision > 0) != hot.grant.is_some() {
        return Err(invalid());
    }
    let holds_task = row.get("holds_task");
    let offer_expiry = row
        .get::<i64, _>("offer_expires_at_ms")
        .try_into()
        .map_err(|_| invalid())?;
    let preparation = if hot.status == Status::Preparing {
        Some(receipts::preparation(tx, &binding, &hot, request.identity.node_id).await?)
    } else {
        None
    };
    let started = if hot.grant.is_some() {
        receipts::started(tx, &binding, &hot).await?
    } else {
        None
    };
    let terminal = if hot.status == Status::NeverGranted {
        Some(receipts::never_granted(tx, &binding).await?)
    } else if matches!(hot.stop, Stop::Stopped { .. }) {
        Some(receipts::stopped(tx, &binding, &hot).await?)
    } else {
        None
    };
    Ok(Snapshot {
        binding,
        versions,
        hot,
        holds_task,
        offer_expiry,
        preparation,
        started,
        terminal,
    })
}
const COLUMNS: &str = "SELECT a.attempt_id,a.offer_id,a.session_id,a.holds_task,l.launch_binding,l.manifest_digest,l.execution_spec_digest,l.offer_expires_at_ms,h.state AS hot,h.record_revision,h.authority_revision,h.grant_revision FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) JOIN profiled_authority_hot h USING(attempt_id)";
async fn read(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Reconcile,
    nonce: Uuid,
) -> Result<ReconcilePage, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='1s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(db)?;
    }
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    // This is current incarnation identity only, not a launch-eligibility check.
    let current_boot = match super::registration::admitted_runtime_locked(
        &mut tx,
        auth.id(),
        request.identity.boot_id,
    )
    .await
    {
        Ok(_) => true,
        Err(AppError::Conflict(_)) => false,
        Err(e) => return Err(e),
    };
    let explicit = !request.attempts.is_empty();
    let rows = if explicit {
        let ids = request
            .attempts
            .iter()
            .map(|a| a.binding.attempt_id)
            .collect::<Vec<_>>();
        sqlx::query(&format!("{COLUMNS} WHERE a.node_id=$1 AND a.kind='profiled_v1' AND a.attempt_id=ANY($2) AND l.launch_binding#>>'{{session,boot_id}}'=$3 ORDER BY a.attempt_id LIMIT 32"))
            .bind(auth.id()).bind(ids).bind(request.identity.boot_id.to_string()).fetch_all(&mut *tx).await.map_err(db)?
    } else {
        sqlx::query(&format!("{COLUMNS} WHERE a.node_id=$1 AND a.kind='profiled_v1' AND ($2::UUID IS NULL OR a.attempt_id>$2) AND l.launch_binding#>>'{{session,boot_id}}'=$3 ORDER BY a.attempt_id LIMIT $4"))
            .bind(auth.id()).bind(request.after_attempt).bind(request.identity.boot_id.to_string()).bind(request.limit as i64).fetch_all(&mut *tx).await.map_err(db)?
    };
    let next_after = if !explicit && rows.len() == request.limit as usize {
        rows.last().map(|r| r.get::<Uuid, _>("attempt_id"))
    } else {
        None
    };
    let mut snapshots = std::collections::BTreeMap::new();
    for row in rows {
        let id = row.get::<Uuid, _>("attempt_id");
        let expected = request.attempts.iter().find(|a| a.binding.attempt_id == id);
        // SQL errors abort the transaction and cannot be concealed as row data.
        let result = snapshot(&mut tx, row, request, expected).await;
        if matches!(
            &result,
            Err(AppError::UpdatesUnavailable(_) | AppError::Internal)
        ) {
            return Err(result.err().unwrap());
        }
        snapshots.insert(id, result);
    }
    let time = now(state)?;
    let mut attempts = Vec::new();
    if explicit {
        for wanted in &request.attempts {
            let id = wanted.binding.attempt_id;
            let code = if snapshots.contains_key(&id) {
                FailureCode::RecoveryRequired
            } else {
                FailureCode::UnknownOrNotOwned
            };
            attempts.push(
                match snapshots
                    .remove(&id)
                    .and_then(|v| v.ok())
                    .and_then(|s| s.render(time, current_boot).ok())
                {
                    Some(authority) => ReconcileEntry::Current {
                        authority: Box::new(authority),
                    },
                    None => ReconcileEntry::Refused {
                        attempt_id: id,
                        code,
                    },
                },
            );
        }
    } else {
        for (_, value) in snapshots {
            if let Ok(authority) = value.and_then(|s| s.render(time, current_boot)) {
                attempts.push(ReconcileEntry::Current {
                    authority: Box::new(authority),
                });
            }
        }
    }
    let page = ReconcilePage {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        delivery_nonce: nonce,
        server_time_ms: time,
        attempts,
        next_after,
    };
    page.validate_for(request, nonce).map_err(|_| invalid())?;
    tx.commit().await.map_err(db)?;
    Ok(page)
}
pub async fn reconcile(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Reconcile,
    nonce: Uuid,
) -> Result<ReconcilePage, AppError> {
    TransportDocument::Reconcile(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native reconciliation document".into()))?;
    if request.attempts.len() > request.limit as usize
        || (!request.attempts.is_empty() && request.after_attempt.is_some())
        || nonce.is_nil()
    {
        return Err(AppError::BadRequest(
            "invalid reconciliation batch or nonce".into(),
        ));
    }
    if auth.id() != request.identity.node_id {
        return Err(AppError::Forbidden("reconciliation node mismatch".into()));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        read(state, auth, request, nonce),
    )
    .await
    .map_err(|_| {
        AppError::UpdatesUnavailable(
            "reconciliation deadline exceeded; history remains unchanged".into(),
        )
    })?
}
