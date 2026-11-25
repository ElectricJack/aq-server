use super::*;
type Refusal = (CandidateState, &'static str);
pub(super) struct Composition<'a> {
    pub(super) candidate: &'a CoreCandidate<'a>,
    pub(super) harness: &'a InstalledHarness,
    pub(super) launch: &'a CoreLaunch<'a>,
    pub(super) credential: Option<&'a AuthorizedCredential>,
}
impl Composition<'_> {
    fn validate_placement(&self, context: &Evaluation<'_>) -> CandidateResult {
        let Evaluation {
            input,
            profile: execution,
            ..
        } = context;
        let harness = self.harness;
        if !input.allowed_environments.iter().any(|e| {
            e == harness.environment.id.as_str() || e == &profile_key(&harness.environment)
        }) {
            return Err((CandidateState::PolicyDenied, input.environment_denied));
        }
        if !fresh(
            input.installation.observed_at_ms,
            input.installation.expires_at_ms,
            input.now_ms,
        ) {
            return Err((CandidateState::StaleUnknown, input.installation_stale));
        }
        if let Some(placement) = input.placement {
            crate::placement::check(placement, harness)?;
        }
        crate::placement::admit_confinement(input.placement, &execution.confinement, harness)?;
        Ok(())
    }
    fn delivery<'a>(
        &'a self,
        input: &CoreInputs<'_>,
    ) -> std::result::Result<(&'a AuthorizedCredential, Option<CredentialDelivery>), Refusal> {
        let credential = self.credential.ok_or((
            CandidateState::MissingCredentials,
            "credential_profile_unregistered",
        ))?;
        if !credential.enabled {
            return Err((CandidateState::PolicyDenied, "credential_profile_disabled"));
        }
        credential_backend_policy(&credential.allowed_backends, input.backend_kind)
            .map_err(|reason| (CandidateState::PolicyDenied, reason))?;
        if !credential.provisionable {
            return Err((
                CandidateState::MissingCredentials,
                "credential_not_provisionable",
            ));
        }
        let missing = if credential.material_owner.is_some() {
            "credential_preparation_missing_or_stale"
        } else {
            "installed_credential_missing_or_stale"
        };
        let delivery =
            (input.delivery)(credential).ok_or((CandidateState::MissingCredentials, missing))?;
        Ok((credential, delivery))
    }
    fn reasoning(&self, policy: &SelectionPolicy) -> std::result::Result<String, Refusal> {
        let harness = self.harness;
        let candidate = self.candidate;
        require_capabilities(&policy.required_capabilities, &harness.capabilities).map_err(
            |_| {
                (
                    CandidateState::Unsupported,
                    "required_capability_unsupported",
                )
            },
        )?;
        // The candidate's stated reasoning, as stated, when the
        // harness accepts it; never a substitute.
        aq_profiles::roles::harness_supports(&harness.harness, candidate.reasoning)
            .then(|| candidate.reasoning.to_string())
            .ok_or((
                CandidateState::Unsupported,
                "model_or_reasoning_unsupported",
            ))
    }
    fn resources(
        &self,
        input: &CoreInputs<'_>,
        policy: &SelectionPolicy,
    ) -> std::result::Result<ResourceCeilings, Refusal> {
        let harness = self.harness;
        let launch = self.launch;
        let limits = &harness.limits;
        if !limits.supports_output(policy.max_output_tokens) {
            return Err((
                CandidateState::Unsupported,
                "required_native_limit_unsupported",
            ));
        }
        let mut resources = input.ceilings.clone();
        resources.context_bytes = resources
            .context_bytes
            .min(limits.context_bytes)
            .min(launch.context_limit_bytes);
        resources.wall_time_ms = resources.wall_time_ms.min(limits.wall_time_ms);
        resources.memory_bytes = resources.memory_bytes.min(limits.memory_bytes);
        resources.cpu_millis = resources.cpu_millis.min(limits.cpu_millis);
        resources.process_limit = resources.process_limit.min(limits.process_limit);
        let bytes = launch.startup_bytes;
        if bytes > resources.context_bytes {
            return Err((
                CandidateState::Unsupported,
                "required_context_limit_unsupported",
            ));
        }
        Ok(resources)
    }
    pub(super) fn check(
        &self,
        context: &Evaluation<'_>,
        refusal: Option<&'static str>,
        row: &mut CandidateTrace,
    ) -> std::result::Result<Option<ConcreteChoice>, Refusal> {
        let Evaluation { input, policy, .. } = context;
        let Self {
            candidate,
            harness,
            launch,
            ..
        } = *self;
        if let Some(reason) = refusal {
            return Err((CandidateState::PolicyDenied, reason));
        }
        self.validate_placement(context)?;
        let (credential, delivery) = self.delivery(input)?;
        let native_reasoning = self.reasoning(policy)?;
        let resources = self.resources(input, policy)?;
        let estimate = input.estimates.iter().find(|e| {
            e.credential_profile == credential.profile
                && e.provider == candidate.provider
                && e.model == candidate.model
        });
        (input.cost)(harness, estimate)?;
        let (state, reasons, ids) = availability(input, credential, candidate.model, policy)
            .map_err(|_| (CandidateState::PolicyDenied, "observation_invalid"))?;
        row.state = state;
        row.reason_codes = reasons;
        row.observation_ids = ids;
        if state != CandidateState::Eligible {
            return Ok(None);
        }
        if delivery.is_some() {
            row.reason_codes
                .push("material_preparation_required".into());
        }
        Ok(Some(ConcreteChoice {
            candidate_id: row.candidate_id.clone(),
            profile_candidate_id: candidate.id.clone(),
            provider: candidate.provider.into(),
            model: candidate.model.into(),
            native_reasoning,
            harness: harness.harness.clone(),
            cli_version: harness.cli_version.clone(),
            adapter: harness.adapter.clone(),
            environment: harness.environment.clone(),
            qualification: harness.qualification.clone(),
            failure_classifier: harness.failure_classifier.clone(),
            credential_profile: credential.profile.clone(),
            credential_delivery: delivery,
            account_reference: credential.account_reference.clone(),
            endpoint_reference: credential.endpoint_reference.clone(),
            launch_digest: launch.digest.clone(),
            resources,
            max_output_tokens: policy.max_output_tokens,
            estimate: estimate.cloned(),
        }))
    }
}

impl NativeLimits {
    fn supports_output(&self, output: u64) -> bool {
        self.context_bytes > 0
            && self.wall_time_ms > 0
            && self.memory_bytes > 0
            && self.cpu_millis > 0
            && self.process_limit > 0
            && self.max_output_tokens >= output
    }
}
