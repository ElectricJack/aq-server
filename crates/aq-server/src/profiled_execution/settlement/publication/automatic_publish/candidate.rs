use super::*;

pub(super) struct Publication {
    pub(super) result: results::RecordedResult,
    pub(super) source_op: Uuid,
    pub(super) target: PublicationTarget,
    checkpoint: String,
}

impl Publication {
    pub(super) async fn load(state: &AppState, report_id: Uuid) -> Result<Option<Self>> {
        let mut tx = state.store.pool.begin().await.map_err(db)?;
        let r = results::load_locked(state, &mut tx, report_id).await?;
        let source = crate::source_retention::first_confirmed_locked(&mut tx, &r).await?;
        tx.rollback().await.map_err(db)?;
        let Some((source_op, confirmed)) = source else {
            return Ok(None);
        };
        let admitted = &r.spec.source;
        let project = admitted.project_id;
        let admitted_identity = (
            admitted.repository_id,
            admitted.repository_binding_generation,
        );
        let binding = crate::project_bindings::current(state, project)
            .await?
            .binding;
        let Some(policy) = binding.publication else {
            tracing::info!(%report_id, %project, "delivery required but the project names no publication policy; held");
            return Ok(None);
        };
        let current = (binding.repository_id, binding.repository_binding_generation);
        let retained = &confirmed.manifest.source;
        let saved = (retained.repository_id, retained.binding_generation);
        let same_repositories = current == admitted_identity && saved == admitted_identity;
        if !(binding.enabled && same_repositories) {
            tracing::warn!(%report_id, %project, "publication binding differs from the admitted source; held");
            return Ok(None);
        }
        if policy.mode == PublicationMode::PullRequest {
            tracing::warn!(%report_id, %project, "pull_request publication is unsupported with aq-git 0.2.0 (D0/D2); held, never degraded");
            return Ok(None);
        }
        let target = PublicationTarget::new(
            report_id,
            project,
            Repository {
                id: admitted_identity.0,
                generation: admitted_identity.1,
            },
            policy,
        );
        Ok(Some(Self {
            result: r,
            source_op,
            target,
            checkpoint: confirmed.manifest.checkpoint.hex,
        }))
    }
    pub(super) async fn prepare(
        &self,
        state: &AppState,
        delivery: &DeliveryService,
    ) -> Result<Option<Prepared<'_>>> {
        let Some(base) = self.base(state, delivery).await? else {
            return Ok(None);
        };
        let Some(candidate) = self.candidate(delivery, &base).await? else {
            return Ok(None);
        };
        Ok(Some(Prepared {
            publication: self,
            base,
            candidate,
        }))
    }
    // Candidate base and expected_old are one OID: replay the frozen base before reading a head.
    async fn base(&self, state: &AppState, delivery: &DeliveryService) -> Result<Option<ObjectId>> {
        let report_id = self.target.report_id;
        if let Some(inputs) = delivery
            .frozen_inputs(self.target.candidate_op)
            .await
            .map_err(|_| unavailable())?
        {
            return serde_json::from_value(inputs["base"].clone())
                .map(Some)
                .map_err(|_| conflict());
        }
        let admitted = &self.result.spec.source;
        if let Some(head) = current_head(
            &state.store.pool,
            admitted.project_id,
            admitted.repository_id,
            admitted.repository_binding_generation,
            &self.target.policy.target,
        )
        .await?
        {
            return Ok(Some(sha1(&head.oid)));
        }
        // A handoff successor's admitted base is its predecessor's saved commit; the
        // target still holds the lineage's original base (review C-1).
        let mut conn = state.store.pool.acquire().await.map_err(db)?;
        let base = aq_machine_control::lineage_base(
            &mut conn,
            self.result.spec.task.task_id,
            &admitted.resolved_object.hex,
        )
        .await?;
        if base.is_none() {
            tracing::warn!(%report_id, "lineage history past its bound; held");
        }
        Ok(base.as_deref().map(sha1))
    }
    async fn candidate(
        &self,
        delivery: &DeliveryService,
        base: &ObjectId,
    ) -> Result<Option<Candidate>> {
        let report_id = self.target.report_id;
        // Candidate creation has no remote effect; it may run before the fence.
        let checkpoint = SavedCheckpoint {
            task: self.result.spec.task.task_id,
            source: self.source_op,
            oid: &self.checkpoint,
        };
        let created = delivery
            .create_candidate(checkpoint.request(&self.target, base))
            .await;
        let candidate = match created {
            Ok(receipt) => match receipt.outcome {
                Ok(candidate) => Candidate::from_receipt(candidate).ok_or_else(conflict)?,
                Err(e) => {
                    tracing::warn!(%report_id, code = %e.code, "candidate creation failed; held");
                    return Ok(None);
                }
            },
            Err(e) => {
                tracing::warn!(%report_id, error = %e, "candidate creation unavailable; held");
                return Ok(None);
            }
        };

        Ok(Some(candidate))
    }
}
fn sha1(hex: &str) -> ObjectId {
    ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.to_owned(),
    }
}
