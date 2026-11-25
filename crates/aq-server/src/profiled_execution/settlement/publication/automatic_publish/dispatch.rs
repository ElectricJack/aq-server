use super::*;

impl Prepared<'_> {
    pub(super) async fn publish(
        &self,
        state: &AppState,
        delivery: &DeliveryService,
    ) -> Result<Step> {
        let publication = self.publication;
        let report_id = publication.target.report_id;
        // Acceptance is re-checked under the common locks immediately before any
        // publish request, and the fence then keeps cancellation and abandonment
        // from interleaving with the publication.
        match fence(
            state,
            report_id,
            publication.source_op,
            &self.evidence(Some("publishing"), None, None),
        )
        .await?
        {
            Fence::Fenced => {}
            Fence::Recorded => return Ok(Step::Recorded),
            Fence::NotAccepted => {
                tracing::info!(%report_id, "acceptance no longer holds (cancelled, revoked, superseded or released); not published");
                return Ok(Step::NotAccepted);
            }
        }

        let recorded = match &self.candidate {
            Candidate::Conflicted(conflicted) => Recorded::Unpublished(conflicted.clone()),
            Candidate::Ready(candidate) => {
                let Some(recorded) = self.dispatch(delivery, candidate).await? else {
                    release_fence(
                        state,
                        publication.result.report.binding.attempt_id,
                        publication.source_op,
                    )
                    .await?;
                    return Ok(Step::Held);
                };
                recorded
            }
        };
        self.record(state, recorded).await
    }
    async fn dispatch(
        &self,
        delivery: &DeliveryService,
        candidate_oid: &ObjectId,
    ) -> Result<Option<Recorded>> {
        let publication = self.publication;
        let report_id = publication.target.report_id;
        let publish_op = publication.target.publish_op;
        let outcome = match delivery
            .deliver(
                &publication.target.policy,
                DeliverRequest {
                    context: publication.target.context(publish_op),
                    candidate_id: publication.target.candidate_op,
                    expected_old: self.base.clone(),
                },
                &NoGitHub,
            )
            .await
        {
            Ok(o) => o,
            // A reply that fails validation after dispatch, or a lost receipt
            // write, proves nothing about the remote: the fence stays.
            Err(
                e @ (DeliveryError::Protocol
                | DeliveryError::Database(_)
                | DeliveryError::Json(_)
                | DeliveryError::Transport
                | DeliveryError::Indeterminate),
            ) => {
                let reason = json!({"reason":"publish_reply_unknown","error":e.to_string(),"operation_id":publish_op,"next":"operation.reconcile under a new operation ID; never a re-push"});
                return Ok(Some(Recorded::Indeterminate(reason)));
            }
            Err(e) => {
                // Refused before any intent or dispatch: release the fence.
                tracing::warn!(%report_id, error = %e, "publication refused before dispatch; held");
                return Ok(None);
            }
        };
        match PublicationStatus::from_outcome(candidate_oid, outcome).map_err(|_| conflict())? {
            PublicationStatus::Recorded(recorded) => return Ok(Some(recorded)),
            PublicationStatus::DestinationBlocked => {
                tracing::info!(%report_id, "publication destination is held by another operation; held");
            }
            PublicationStatus::RecoveryBlocked(message) => {
                tracing::warn!(%report_id, message = %message, "aq-git recovery_blocked; held");
            }
            PublicationStatus::Failed(error) => {
                tracing::warn!(%report_id, code = %error, "publication failed before dispatch; held");
            }
        }
        Ok(None)
    }
}
