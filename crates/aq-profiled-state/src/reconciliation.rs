//! Read-only phase and release evidence for the exact recorded launch.
use crate::{Effects, Retention, Status, Stop, hot::HotState};
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding,
    control::{CurrentAuthority, CurrentPhase, FailureCode, LaunchOwner, Versions},
    transport::PreparationPlan,
};
use sqlx::PgConnection;
use uuid::Uuid;

fn owner(hot: &HotState) -> Option<LaunchOwner> {
    hot.owner.as_ref().map(|o| LaunchOwner {
        reservation_id: o.reservation_id,
        guardian_id: o.guardian_id,
        launch_id: o.launch_id,
    })
}
pub struct Snapshot {
    pub binding: LaunchBinding,
    pub versions: Versions,
    pub hot: HotState,
    pub holds_task: bool,
    pub offer_expiry: u64,
    pub preparation: Option<(PreparationPlan, Digest)>,
    pub started: Option<Uuid>,
    pub terminal: Option<ArtifactRef>,
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
    pub fn render(self, now: u64, current_boot: bool) -> Option<CurrentAuthority> {
        let phase = if self.hot.status == Status::NeverGranted {
            CurrentPhase::NeverGranted {
                closure: self.terminal.clone()?,
            }
        } else if !self.holds_task {
            CurrentPhase::Released {
                release: self.terminal.clone()?,
            }
        } else if matches!(self.hot.stop, Stop::Stopped { .. }) {
            CurrentPhase::Stopped {
                owner: owner(&self.hot)?,
                grant_id: self.hot.grant.as_ref()?.grant_id,
                stopped_receipt: self.terminal.clone()?,
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
                    let (plan, digest) = self.preparation.as_ref()?;
                    if plan.clock.expires_at_ms <= now {
                        self.stop(FailureCode::Expired)
                    } else {
                        CurrentPhase::Preparing {
                            owner: owner(&self.hot)?,
                            preparation_operation_id: plan.operation_id,
                            preparation_digest: digest.clone(),
                            expires_at_ms: plan.clock.expires_at_ms,
                        }
                    }
                }
                Status::Granted => {
                    let grant = self.hot.grant.as_ref()?;
                    if grant.expires_at_ms <= now {
                        self.stop(FailureCode::Expired)
                    } else {
                        CurrentPhase::Granted {
                            owner: owner(&self.hot)?,
                            grant_id: grant.grant_id,
                            expires_at_ms: grant.expires_at_ms,
                            execution_deadline_ms: self.hot.execution_deadline_ms?,
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
        authority.validate(now).ok()?;
        Some(authority)
    }
}
/// The server settled an expired grant as never started and released its exact claim.
pub async fn lapsed_release(
    database: &mut PgConnection,
    attempt: Uuid,
    hot: &HotState,
    receipt_prefix: &str,
) -> sqlx::Result<Option<ArtifactRef>> {
    let (
        Stop::NotStarted { receipt: stop },
        Retention::Retained { receipt: source },
        Effects::Quiescent { receipt: effects },
    ) = (&hot.stop, &hot.retention, &hot.effects)
    else {
        return Ok(None);
    };
    if hot.status != Status::Finished
        || !hot.revoked
        || hot.grant.is_none()
        || stop.id.as_str() != format!("{receipt_prefix}{attempt}")
        || stop.version.as_str() != "1"
    {
        return Ok(None);
    }
    let released = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM profiled_replacement_releases r JOIN execution_attempts a USING(attempt_id)
           WHERE a.attempt_id=$1 AND r.task_id=a.task_id AND NOT a.holds_task
           AND r.reason='failed_attempt_replacement'
           AND r.stop_receipt_digest=$2 AND r.retained_source_receipt_digest=$3
           AND r.effect_reconciliation_receipt_digest=$4
           AND NOT EXISTS(SELECT 1 FROM profiled_started_receipts s WHERE s.attempt_id=$1))"
    )
    .bind(attempt).bind(stop.digest.as_str()).bind(source.digest.as_str()).bind(effects.digest.as_str())
    .fetch_one(database).await?;
    Ok(released.then(|| stop.clone()))
}
