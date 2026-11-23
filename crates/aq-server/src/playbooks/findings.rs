//! AQ owns health baselines and episodes. Only trusted host code can attest
//! coverage; worker input never supplies completeness or refreshes a baseline.
use super::*;
use aq_notifications::SendRequest;
use aq_playbook_domain::finding_episodes::{EpisodeChange, Episodes, FindingScan, RetainedScan};
use aq_playbook_domain::health_scans::{HealthScans, ScanEvaluation, ScanStart, bounded};

impl Service {
    /// D8 findings run on Monitoring6 and, since D11, on Host7/Host8 (catalog
    /// 2.0.0/2.1.0 findings.reconcile is byte-identical to 1.0.0).
    fn findings_profile(&self) -> Result<()> {
        if !self.profile.has_findings() {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) async fn capture_health_scan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        request: ScanStart<'_>,
    ) -> Result<BTreeMap<String, i64>> {
        self.findings_profile()?;
        HealthScans(&self.namespace)
            .capture(tx, request, || self.now())
            .await
    }
    /// Trusted scan-start boundary, before reading monitored subjects. Retrying
    /// an observation always returns its original revisions and expiry.
    pub async fn start_health_scan(
        &self,
        owner: &RuntimeOwner,
        observation: &str,
        partition: &str,
        ttl_ms: i64,
    ) -> Result<BTreeMap<String, i64>> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let baseline = self
            .capture_health_scan(
                &mut tx,
                ScanStart {
                    observation,
                    partition,
                    ttl_ms,
                },
            )
            .await?;
        tx.commit().await?;
        Ok(baseline)
    }
    /// Called by the host evaluator after verifying coverage. This is not a
    /// command or HTTP endpoint. A recorded evaluation can never be upgraded.
    pub async fn complete_health_scan(
        &self,
        owner: &RuntimeOwner,
        observation: &str,
        evaluation: &str,
        complete: bool,
    ) -> Result<()> {
        self.findings_profile()?;
        if !bounded(evaluation) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        HealthScans(&self.namespace)
            .complete(
                &mut tx,
                ScanEvaluation {
                    observation,
                    evaluation,
                    complete,
                },
                || self.now(),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn findings_reconcile(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        self.findings_profile()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let request = FindingScan::parse(&intent.input, || self.now())?;
        let scan = request.observation(&mut tx, &self.namespace).await?;
        let hash = digest(&intent.input)?;
        let result = match scan {
            Some(scan) => {
                self.reconcile_scan(
                    &mut tx,
                    request,
                    RetainedScan {
                        namespace: &self.namespace,
                        row: scan,
                        hash,
                    },
                )
                .await?
            }
            None => request.result("refused"),
        };
        self.settle_fresh(tx, command.retained(&identity), result)
            .await
    }

    async fn reconcile_scan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        request: FindingScan<'_>,
        scan: RetainedScan<'_>,
    ) -> Result<HostResult> {
        if let Some(result) = scan.retained(&request)? {
            return Ok(result);
        }
        if !scan.permits(&request, || self.now())? {
            return Ok(request.result("refused"));
        }
        let episodes = scan.episodes(tx, request.partition).await?;
        let outcome = if episodes.conflicts(&request) {
            "conflict"
        } else {
            self.apply_findings(tx, &request, &episodes).await?;
            "reconciled"
        };
        let result = request.result(outcome);
        scan.record(tx, &request, &result).await?;
        Ok(result)
    }

    async fn apply_findings(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        request: &FindingScan<'_>,
        episodes: &Episodes,
    ) -> Result<()> {
        for finding in &request.findings {
            let change = episodes.reported(finding, request)?;
            self.write_episode(tx, request.partition, change).await?;
        }
        for change in episodes.cleared(request) {
            self.write_episode(tx, request.partition, change?).await?;
        }
        Ok(())
    }
    async fn write_episode(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        partition: &str,
        change: EpisodeChange<'_>,
    ) -> Result<()> {
        let EpisodeChange {
            key,
            revision,
            present,
            evidence,
            notify,
        } = change;
        let now = self.now()?;
        sqlx::query("INSERT INTO playbook_finding_episodes(namespace_id,generation,partition,finding_key,revision,present,evidence,last_notified_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace_id,generation,partition,finding_key) DO UPDATE SET revision=EXCLUDED.revision,present=EXCLUDED.present,evidence=EXCLUDED.evidence,last_notified_ms=CASE WHEN $9 THEN EXCLUDED.last_notified_ms ELSE playbook_finding_episodes.last_notified_ms END")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(partition).bind(key).bind(revision).bind(present).bind(evidence).bind(now).bind(notify).execute(&mut **tx).await?;
        if notify {
            let notification = stable_id(
                "aq.finding.notification",
                self.namespace.namespace_id,
                self.namespace.generation,
                &digest(&(partition, key, revision, now))?,
            )?;
            // The time is part of the identity: a cleared episode is deleted in
            // time, and the finding's next episode starts at revision 1 again.
            // Evidence stays in AQ; the deliberate summary identifies the
            // episode. Delivery routing belongs to the notifications owner.
            let request = SendRequest {
                operation_id: notification,
                message_id: notification,
                revision: 1,
                scope: aq_lifecycle_api::Scope {
                    project_id: self.namespace.project_id,
                    workspace_id: self.namespace.workspace_id,
                },
                recipient_user_id: self.namespace.owner_user_id,
                summary: format!(
                    "Finding {} in {}: {}.",
                    key.chars().filter(|c| !c.is_control()).collect::<String>(),
                    partition
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect::<String>(),
                    if present {
                        "needs attention"
                    } else {
                        "cleared"
                    }
                ),
                ttl_ms: 86_400_000,
            };
            sqlx::query("INSERT INTO playbook_finding_notifications(namespace_id,generation,notification_id,request) VALUES($1,$2,$3,$4)")
                .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(notification).bind(serde_json::to_value(request)?).execute(&mut **tx).await?;
        }
        Ok(())
    }
    /// Submit frozen intents through the existing notifications owner after
    /// the episode transaction. Lost acknowledgements replay the same IDs.
    pub async fn flush_finding_notifications(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        self.findings_profile()?;
        if !(1..=32).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows:Vec<(Uuid,Value)>=sqlx::query_as("SELECT notification_id,request FROM playbook_finding_notifications WHERE namespace_id=$1 AND generation=$2 AND NOT submitted ORDER BY sequence LIMIT $3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let service = crate::notifications::api::service(&self.ports()?.state);
        for (id, request) in &rows {
            service
                .enqueue(
                    self.namespace.owner_user_id,
                    serde_json::from_value(request.clone())?,
                )
                .await
                .map_err(|error| {
                    // The immutable intent is still pending, including when
                    // its scope is temporarily unavailable or enqueue's
                    // outcome is unknown. Its fixed IDs make retry safe.
                    tracing::warn!(%error, "finding notification remains pending");
                    Error::Overloaded
                })?;
            let mut tx = self.tx().await?;
            let ns = self.namespace_locked(&mut tx).await?;
            self.runtime(&ns, owner)?;
            sqlx::query("UPDATE playbook_finding_notifications SET submitted=true WHERE namespace_id=$1 AND generation=$2 AND notification_id=$3")
                .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Ok(rows.len())
    }
}
