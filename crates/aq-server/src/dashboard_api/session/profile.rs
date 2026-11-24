use super::*;

pub(super) struct LaunchProfile {
    pub profile: Profile,
    pub revisions: Vec<ProfileRevision>,
    pub task_revision: Option<Revision>,
    pub lifecycle: Option<SessionLifecycle>,
    pub observation_binding: Option<aq_lifecycle_api::Binding>,
}

impl LaunchProfile {
    pub(super) async fn read(
        inspected: &Inspected,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Self> {
        if inspected.view.kind != Kind::ProfiledV1 {
            return Ok(Self {
                profile: Profile {
                    provider: None,
                    model: None,
                    harness: None,
                },
                revisions: vec![],
                task_revision: None,
                lifecycle: None,
                observation_binding: None,
            });
        }
        let attempt = inspected.attempt;
        // Reuse the existing authorized per-session offer read: receipt PK joins
        // add bounded columns, never another per-row query or an event scan.
        let row=sqlx::query("SELECT e.state->'offer' AS offer,s.request_bytes AS started_request,s.request_digest AS started_digest,s.receipt_bytes AS started_receipt,z.request_bytes AS stopped_request,z.request_digest AS stopped_digest,z.receipt_bytes AS stopped_receipt FROM execution_attempts e LEFT JOIN profiled_started_receipts s USING(attempt_id) LEFT JOIN profiled_stopped_receipts z USING(attempt_id) WHERE e.attempt_id=$1")
            .bind(attempt).fetch_one(&mut **tx).await?;
        let raw: Value = row.get("offer");
        let offer: ProfiledOffer = serde_json::from_value(raw).map_err(|_| Error::Unavailable)?;
        offer.validate().map_err(|_| Error::Unavailable)?;
        if !matches_offer(&inspected.view, inspected.attempt, &offer) {
            return Err(Error::Unavailable);
        }
        let binding =
            aq_admission::transport::lifecycle_binding(&offer).map_err(|_| Error::Unavailable)?;
        if !inspected.matches_lifecycle(&binding) {
            return Err(Error::Unavailable);
        }
        Self::from_offer(offer, binding, &row)
    }
    fn from_offer(
        offer: ProfiledOffer,
        binding: aq_lifecycle_api::Binding,
        row: &sqlx::postgres::PgRow,
    ) -> Result<Self> {
        let lifecycle = Some(lifecycle::from_row(row, &offer.binding)?);
        let profile = Profile {
            provider: Some(offer.manifest.model.provider.as_str().into()),
            model: Some(offer.manifest.model.model.as_str().into()),
            harness: Some(offer.manifest.harness.harness.as_str().into()),
        };
        let task_revision = Some(Revision(offer.manifest.bindings.task_revision.to_string()));
        let mut revisions = vec![];
        for (name, reference) in [
            ("agent", offer.manifest.agent_profile),
            ("intelligence", offer.manifest.intelligence_profile),
            ("execution", offer.manifest.execution_profile),
        ] {
            if let Some(revision) = decimal(reference.version.as_str()) {
                revisions.push(ProfileRevision {
                    name: name.into(),
                    revision,
                });
            }
        }
        Ok(Self {
            profile,
            revisions,
            task_revision,
            lifecycle,
            observation_binding: Some(binding),
        })
    }
}

pub(in crate::dashboard_api::service) fn matches_offer(
    view: &AuthorityView,
    attempt: Uuid,
    offer: &ProfiledOffer,
) -> bool {
    let binding = &offer.binding;
    binding.session == view.session
        && binding.attempt_id == attempt
        && binding.offer_id == view.offer_id
        && Some(&binding.launch_manifest_digest) == view.launch_manifest_digest.as_ref()
        && binding.execution_spec_digest.as_str() == view.execution_spec_digest
}

impl Inspected {
    fn matches_lifecycle(&self, binding: &aq_lifecycle_api::Binding) -> bool {
        let view = &self.view;
        binding.project_id == view.project_id
            && binding.task_id == view.task_id
            && binding.attempt_id == self.attempt
            && binding.attempt_generation == view.attempt.generation
            && binding.session == view.session
    }
}
