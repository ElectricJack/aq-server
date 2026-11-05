//! Controller wiring for one supervisor turn. No worker credential delivery.
use crate::{PlanAuthority, Supervisor, clock, config::protected_read};
use anyhow::{Context, Result, ensure};
use aq_central_authority::{DispatchState, HandoffDisposition, Phase};
use aq_server::agent_access::supervisor_conversation::reply;
use aq_session_host::HostAdapter;
use std::path::PathBuf;
use uuid::Uuid;
use zeroize::Zeroizing;

impl<A: PlanAuthority, H: HostAdapter + Send + Sync + 'static> Supervisor<A, H> {
    /// After Running, verify handoff and install the generation's reply credential.
    /// Re-adoption keeps the live token so a controller restart cannot revoke the
    /// credential already held by the native process. No bytes leave this API.
    pub async fn enable_replies(&self, actor: Uuid, at_ms: u64) -> Result<PathBuf> {
        self.handoff(actor).await?;
        let _fence = self.fence(actor).await?;
        let s = self.current(actor).await?;
        ensure!(
            s.run.phase() == Phase::RunningReported
                && matches!(s.run.dispatch(), DispatchState::Resolved { evidence }
                    if evidence.disposition == HandoffDisposition::HandedOff),
            "reply credential requires verified handoff"
        );
        let record = self.record(&s).await?;
        let path = self
            .root
            .join(record.id.to_string())
            .join("supervisor.credential");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let bytes = Zeroizing::new(protected_read(&path, 70)?);
                let token = std::str::from_utf8(&bytes)?;
                let principal = reply::authenticate(&self.pool, token, "messaging", at_ms)
                    .await
                    .map_err(|_| anyhow::anyhow!("installed supervisor credential rejected"))?;
                ensure!(
                    principal.actor_id == actor && principal.generation == s.run.plan().generation,
                    "installed credential belongs to another generation"
                );
                Ok(path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let remaining = s
                    .run
                    .original_deadline_boot_ms()
                    .checked_sub(clock()?.boot_ms)
                    .filter(|ttl| *ttl > 0)
                    .context("supervisor generation deadline passed")?;
                let token = Zeroizing::new(
                    reply::issue_credential(
                        &self.pool,
                        actor,
                        remaining.min(24 * 60 * 60 * 1000),
                        at_ms,
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("supervisor credential issuance refused"))?,
                );
                self.install_credential_locked(&s, &token).await
            }
            Err(error) => Err(error.into()),
        }
    }
}
