//! AQ owns health baselines and episodes. Only trusted host code can attest
//! coverage; worker input never supplies completeness or refreshes a baseline.
use super::*;
use aq_notifications::SendRequest;
use std::collections::BTreeSet;

fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\0')
}
impl Service {
    /// D8 findings run on Monitoring6 and, since D11, on Host7/Host8 (catalog
    /// 2.0.0/2.1.0 findings.reconcile is byte-identical to 1.0.0).
    fn findings_profile(&self) -> Result<()> {
        if !matches!(
            self.profile,
            CatalogProfile::Monitoring6 | CatalogProfile::Host7 | CatalogProfile::Host8
        ) {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) async fn capture_health_scan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        observation: &str,
        partition: &str,
        ttl_ms: i64,
    ) -> Result<BTreeMap<String, i64>> {
        self.findings_profile()?;
        if !bounded(observation) || !bounded(partition) || !(1..=60_000).contains(&ttl_ms) {
            return Err(Error::Invalid);
        }
        if let Some(row) = sqlx::query("SELECT partition,baseline FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).fetch_optional(&mut **tx).await? {
            if row.get::<String,_>("partition") != partition { return Err(Error::Conflict); }
            return Ok(serde_json::from_value(row.get("baseline"))?);
        }
        let rows:Vec<(String,i64)> = sqlx::query_as("SELECT finding_key,revision FROM playbook_finding_episodes WHERE namespace_id=$1 AND generation=$2 AND partition=$3 ORDER BY finding_key")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(partition).fetch_all(&mut **tx).await?;
        let baseline: BTreeMap<_, _> = rows.into_iter().collect();
        let value = serde_json::to_value(&baseline)?;
        self.budget(
            tx,
            0,
            0,
            0,
            0,
            serde_json::to_vec(&value)?.len() as i64
                + observation.len() as i64
                + partition.len() as i64
                + 4096,
        )
        .await?;
        sqlx::query("INSERT INTO playbook_health_observations(namespace_id,generation,observation,partition,baseline,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).bind(partition).bind(value).bind(self.now()?.checked_add(ttl_ms).ok_or(Error::Invalid)?).execute(&mut **tx).await?;
        Ok(baseline)
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
            .capture_health_scan(&mut tx, observation, partition, ttl_ms)
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
        let row=sqlx::query("SELECT evaluation,complete,expires_at_ms FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        if let Some(old) = row.get::<Option<String>, _>("evaluation") {
            if old != evaluation || row.get::<bool, _>("complete") != complete {
                return Err(Error::Conflict);
            }
        } else {
            if row.get::<i64, _>("expires_at_ms") <= self.now()? {
                return Err(Error::Forbidden);
            }
            self.budget(&mut tx, 0, 0, 0, 0, evaluation.len() as i64)
                .await?;
            sqlx::query("UPDATE playbook_health_observations SET evaluation=$4,complete=$5 WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
                .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).bind(evaluation).bind(complete).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn findings_reconcile(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        self.findings_profile()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let input = &intent.input;
        let partition = text(input, "partition")?;
        let observation = text(input, "observation")?;
        let evaluation = text(input, "evaluation")?;
        if ![partition, observation, evaluation]
            .into_iter()
            .all(bounded)
        {
            return Err(Error::Invalid);
        }
        let findings = input["findings"].as_array().ok_or(Error::Invalid)?;
        let now = self.now()?;
        let reminder = input["reminder_ms"]
            .as_i64()
            .filter(|v| *v >= 0)
            .ok_or(Error::Invalid)?;
        let mut seen = BTreeSet::new();
        for f in findings {
            if !bounded(text(f, "key")?)
                || (text(f, "evidence")?.is_empty()
                    || text(f, "evidence")?.len() > 4096
                    || text(f, "evidence")?.contains('\0'))
                || !seen.insert(text(f, "key")?)
            {
                return Err(Error::Invalid);
            }
        }
        let result = |outcome: &str| HostResult {
            outcome: outcome.into(),
            output: json!({"reference":observation,"subject":subject(partition,0)}),
        };
        let row=sqlx::query("SELECT * FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).fetch_optional(&mut *tx).await?;
        let hash = digest(input)?;
        let mut outcome = "refused";
        if let Some(scan) = row {
            if let Some(old) = scan.get::<Option<Value>, _>("result") {
                let retained =
                    if scan.get::<Option<String>, _>("request_digest").as_deref() == Some(&hash) {
                        HostResult {
                            outcome: old["outcome"].as_str().ok_or(Error::Unknown)?.into(),
                            output: old["output"].clone(),
                        }
                    } else {
                        result("conflict")
                    };
                return self
                    .settle_fresh(tx, id, revision, intent, &identity, retained)
                    .await;
            }
            if scan.get::<String, _>("partition") == partition
                && scan.get::<Option<String>, _>("evaluation").as_deref() == Some(evaluation)
                && scan.get::<i64, _>("expires_at_ms") > self.now()?
            {
                let baseline: BTreeMap<String, i64> = serde_json::from_value(scan.get("baseline"))?;
                let rows=sqlx::query("SELECT finding_key,revision,present,evidence,last_notified_ms FROM playbook_finding_episodes WHERE namespace_id=$1 AND generation=$2 AND partition=$3")
                    .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(partition).fetch_all(&mut *tx).await?;
                let current: BTreeMap<String, _> = rows
                    .into_iter()
                    .map(|r| (r.get("finding_key"), r))
                    .collect();
                let complete = scan.get::<bool, _>("complete");
                let conflicts = findings.iter().any(|f| {
                    let key = f["key"].as_str().unwrap();
                    let expected = f["expected_revision"].as_i64();
                    expected != Some(*baseline.get(key).unwrap_or(&0))
                        || expected
                            != Some(current.get(key).map(|r| r.get("revision")).unwrap_or(0))
                }) || (complete
                    && current
                        .iter()
                        .any(|(key, r)| baseline.get(key) != Some(&r.get::<i64, _>("revision"))));
                outcome = if conflicts { "conflict" } else { "reconciled" };
                if !conflicts {
                    let new_count = seen.iter().filter(|k| !current.contains_key(**k)).count();
                    if current.len() + new_count > 100 {
                        return Err(Error::Overloaded);
                    }
                    for f in findings {
                        let key = text(f, "key")?;
                        let evidence = text(f, "evidence")?;
                        let old = current.get(key);
                        let rev = old
                            .map(|r| r.get::<i64, _>("revision"))
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or(Error::Overloaded)?;
                        let notify = old.is_none_or(|r| {
                            !r.get::<bool, _>("present")
                                || r.get::<String, _>("evidence") != evidence
                                || (reminder > 0
                                    && now.saturating_sub(r.get("last_notified_ms")) >= reminder)
                        });
                        self.write_episode(&mut tx, partition, key, rev, true, evidence, notify)
                            .await?;
                    }
                    if complete {
                        for (key, old) in &current {
                            if old.get::<bool, _>("present") && !seen.contains(key.as_str()) {
                                self.write_episode(
                                    &mut tx,
                                    partition,
                                    key,
                                    old.get::<i64, _>("revision")
                                        .checked_add(1)
                                        .ok_or(Error::Overloaded)?,
                                    false,
                                    &old.get::<String, _>("evidence"),
                                    true,
                                )
                                .await?;
                            }
                        }
                    }
                }
                let receipt = result(outcome);
                self.budget(&mut tx, 0, 0, 0, 0, 4096).await?;
                sqlx::query("UPDATE playbook_health_observations SET request_digest=$4,result=$5 WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
                    .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(observation).bind(hash).bind(json!({"outcome":receipt.outcome,"output":receipt.output})).execute(&mut *tx).await?;
            }
        }
        self.settle_fresh(tx, id, revision, intent, &identity, result(outcome))
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn write_episode(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        partition: &str,
        key: &str,
        revision: i64,
        present: bool,
        evidence: &str,
        notify: bool,
    ) -> Result<()> {
        let now = self.now()?;
        self.budget(
            tx,
            0,
            0,
            0,
            0,
            (partition.len() + key.len() + evidence.len()) as i64 + 4096,
        )
        .await?;
        sqlx::query("INSERT INTO playbook_finding_episodes(namespace_id,generation,partition,finding_key,revision,present,evidence,last_notified_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace_id,generation,partition,finding_key) DO UPDATE SET revision=EXCLUDED.revision,present=EXCLUDED.present,evidence=EXCLUDED.evidence,last_notified_ms=CASE WHEN $9 THEN EXCLUDED.last_notified_ms ELSE playbook_finding_episodes.last_notified_ms END")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(partition).bind(key).bind(revision).bind(present).bind(evidence).bind(now).bind(notify).execute(&mut **tx).await?;
        if notify {
            let notification = stable_id(
                "aq.finding.notification",
                self.namespace.namespace_id,
                self.namespace.generation,
                &digest(&(partition, key, revision))?,
            )?;
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
            self.budget(tx, 0, 0, 0, 0, 4096).await?;
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
