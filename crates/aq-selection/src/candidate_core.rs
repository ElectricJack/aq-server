//! One subject-neutral candidate evaluator. Domain wrappers own admission facts.
use crate::validate::fresh;
use crate::*;
use aq_profiles::{FallbackReason, SelectionPolicy};
use aq_session_protocol::require_capabilities;
mod composition;
mod observations;
use composition::Composition;
use observations::availability;
pub(crate) use observations::influencing_expiry;
pub(crate) enum ObservationTarget {
    Worker {
        node_id: uuid::Uuid,
        boot_id: uuid::Uuid,
    },
    CentralInstallation,
}
/// One prepared launch: a startup text for a harness and environment.
pub(crate) struct CoreLaunch<'a> {
    pub digest: &'a Digest,
    pub harness: &'a str,
    pub environment: &'a str,
    pub context_limit_bytes: u64,
    pub startup_bytes: u64,
}
/// One candidate of an intelligence profile.
pub(crate) struct CoreCandidate<'a> {
    /// Its identity in decision records.
    pub id: String,
    pub provider: &'a str,
    pub model: &'a str,
    /// The reasoning effort the candidate states (spec 0014 §5.2). It is used
    /// as stated, or the candidate is unsupported on a harness that does not
    /// accept it.
    pub reasoning: &'a str,
    pub harnesses: &'a [String],
}
impl<'a> CoreProfile<'a> {
    /// The launch a role snapshot resolved: each candidate states its
    /// reasoning, and the execution profile names one environment and one
    /// credential profile per harness.
    pub fn launch(launch: &'a aq_profiles::roles::Launch<'a>) -> Self {
        Self {
            candidates: (launch.candidates.iter())
                .map(|c| CoreCandidate {
                    id: c.derived_id(),
                    provider: &c.provider,
                    model: &c.model,
                    reasoning: &c.reasoning,
                    harnesses: &c.harnesses,
                })
                .collect(),
            harnesses: &launch.execution.harnesses,
            environments: launch.execution.pinned_environments(),
            credentials: &launch.execution.credentials,
            confinement: launch.execution.confinement.execution_confinement(),
        }
    }
}
/// The candidates in preference order and where they may run.
pub(crate) struct CoreProfile<'a> {
    pub candidates: Vec<CoreCandidate<'a>>,
    pub harnesses: &'a [String],
    pub environments: &'a [String],
    pub credentials: &'a std::collections::BTreeMap<String, String>,
    pub confinement: aq_profiles::ExecutionConfinement,
}
/// A credential profile reference names a profile by `id@version` or by id.
fn names_profile(reference: &str, profile: &aq_session_protocol::ArtifactRef) -> bool {
    reference == profile_key(profile) || reference == profile.id.as_str()
}
pub(crate) struct CoreInstallation<'a> {
    pub harnesses: &'a [InstalledHarness],
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
pub(crate) type CandidateResult = std::result::Result<(), (CandidateState, &'static str)>;
pub(crate) type DeliveryCheck<'a> =
    dyn Fn(&AuthorizedCredential) -> Option<Option<CredentialDelivery>> + 'a;
pub(crate) type CostCheck<'a> =
    dyn Fn(&InstalledHarness, Option<&CandidateEstimate>) -> CandidateResult + 'a;
pub(crate) struct CoreInputs<'a> {
    pub backend_kind: Option<BackendKind>,
    pub target: ObservationTarget,
    pub installation: CoreInstallation<'a>,
    pub credentials: &'a [AuthorizedCredential],
    pub observations: &'a [AvailabilityObservation],
    pub estimates: &'a [CandidateEstimate],
    pub launches: &'a [CoreLaunch<'a>],
    pub ceilings: &'a ResourceCeilings,
    pub allowed_environments: &'a [String],
    pub placement: Option<&'a Placement>,
    pub route: Option<&'a RouteLimits>,
    pub now_ms: u64,
    pub global: Option<&'static str>,
    pub environment_denied: &'static str,
    pub installation_stale: &'static str,
    pub delivery: &'a DeliveryCheck<'a>,
    pub cost: &'a CostCheck<'a>,
}
pub(crate) struct CoreOutcome {
    pub selected: Option<(ConcreteChoice, u64, usize)>,
    pub traces: Vec<CandidateTrace>,
    pub reason_codes: Vec<String>,
}
pub(crate) fn profile_key(profile: &aq_session_protocol::ArtifactRef) -> String {
    format!("{}@{}", profile.id.as_str(), profile.version.as_str())
}
struct Evaluation<'a> {
    profile: &'a CoreProfile<'a>,
    policy: &'a SelectionPolicy,
    input: &'a CoreInputs<'a>,
}
mod ranking;
use ranking::{EvaluatedRow, rank};

pub(crate) fn evaluate(
    profile: &CoreProfile<'_>,
    policy: &SelectionPolicy,
    input: &CoreInputs<'_>,
    unavailable: &std::collections::BTreeMap<String, &'static str>,
) -> Result<CoreOutcome> {
    let context = Evaluation {
        input,
        policy,
        profile,
    };
    let mut rows = Vec::new();
    let mut harnesses: Vec<_> = input.installation.harnesses.iter().collect();
    harnesses.sort_by_key(|h| {
        (
            &h.harness,
            &h.environment.digest,
            &h.adapter.digest,
            &h.cli_version,
        )
    });
    for candidate in &profile.candidates {
        context.candidate_rows(candidate, &harnesses, &mut rows)?;
    }
    for row in &mut rows {
        if row.choice.is_some()
            && let Some(reason) = unavailable.get(&row.trace.provider)
        {
            row.choice = None;
            row.trace.state = CandidateState::Unavailable;
            row.trace.reason_codes = vec![(*reason).into()];
        }
    }
    Ok(rank(rows, input.route, policy))
}

impl Evaluation<'_> {
    fn prepared_harness<'a>(
        &'a self,
        name: &str,
        harness: &'a InstalledHarness,
    ) -> Option<(&'a InstalledHarness, &'a CoreLaunch<'a>)> {
        if harness.harness != name
            || !self
                .profile
                .environments
                .iter()
                .any(|e| names_profile(e, &harness.environment))
        {
            return None;
        }
        let launch =
            self.input.launches.iter().find(|l| {
                l.harness == name && names_profile(l.environment, &harness.environment)
            })?;
        Some((harness, launch))
    }
    fn candidate_rows(
        &self,
        candidate: &CoreCandidate<'_>,
        harnesses: &[&InstalledHarness],
        rows: &mut Vec<EvaluatedRow>,
    ) -> Result<()> {
        let before = rows.len();
        for name in self
            .profile
            .harnesses
            .iter()
            .filter(|name| candidate.harnesses.contains(name))
        {
            let reference = self
                .profile
                .credentials
                .get(name)
                .map_or("unconfigured", String::as_str);
            if self
                .input
                .credentials
                .iter()
                .any(|c| names_profile(reference, &c.profile) && c.provider != candidate.provider)
            {
                continue;
            }
            let credential =
                self.input.credentials.iter().find(|c| {
                    names_profile(reference, &c.profile) && c.provider == candidate.provider
                });
            for (harness, launch) in harnesses
                .iter()
                .filter_map(|h| self.prepared_harness(name, h))
            {
                ensure(rows.len() < 128, "candidate fanout exceeds published bound")?;
                let composition = Composition {
                    candidate,
                    harness,
                    launch,
                    credential,
                };
                rows.push(composition.row(self, reference)?);
            }
        }
        if before == rows.len() {
            ensure(rows.len() < 128, "candidate fanout exceeds published bound")?;
            rows.push(candidate.unavailable(self.input.route)?);
        }
        Ok(())
    }
}

impl CoreCandidate<'_> {
    fn trace(&self, id: String, harness: &str, environment: String) -> CandidateTrace {
        CandidateTrace {
            candidate_id: id,
            profile_candidate_id: self.id.clone(),
            provider: self.provider.into(),
            model: self.model.into(),
            harness: harness.into(),
            environment,
            credential_profile: None,
            state: CandidateState::Eligible,
            reason_codes: vec!["candidate_evaluated".into()],
            observation_ids: vec![],
        }
    }
    fn unavailable(&self, route: Option<&RouteLimits>) -> Result<EvaluatedRow> {
        let id = format!(
            "choice:{}",
            digest(&(&self.id, "no-compatible-composition"))?.as_str()
        );
        // When the harness is unknown, only the route's provider allowlist applies.
        let excluded = route.and_then(|r| crate::route::excludes_provider(r, self.provider));
        let (state, reason) = match excluded {
            Some(reason) => (CandidateState::PolicyDenied, reason),
            None => (
                CandidateState::Unsupported,
                "compatible_installed_composition_unavailable",
            ),
        };
        let mut trace = self.trace(id, "unavailable", "unavailable".into());
        trace.state = state;
        trace.reason_codes = vec![reason.into()];
        Ok(EvaluatedRow {
            trace,
            choice: None,
            revision: 0,
            excluded: excluded.is_some(),
        })
    }
}

impl Composition<'_> {
    fn row(&self, context: &Evaluation<'_>, reference: &str) -> Result<EvaluatedRow> {
        let Self {
            candidate,
            harness,
            credential,
            ..
        } = *self;
        let id = format!(
            "choice:{}",
            digest(&(
                candidate.id.as_str(),
                &harness.adapter,
                &harness.environment,
                &harness.cli_version,
                reference
            ))?
            .as_str()
        );
        let mut trace = candidate.trace(id, &harness.harness, profile_key(&harness.environment));
        trace.credential_profile = credential.map(|c| c.profile.clone());
        // A global refusal outranks route exclusion.
        let input = context.input;
        let excluded = input
            .route
            .filter(|_| input.global.is_none())
            .and_then(|r| crate::route::excludes(r, &harness.harness, candidate.provider));
        let choice = match self.check(context, input.global.or(excluded), &mut trace) {
            Ok(choice) => choice,
            Err((state, reason)) => {
                trace.state = state;
                trace.reason_codes = vec![reason.into()];
                None
            }
        };
        Ok(EvaluatedRow {
            trace,
            choice,
            revision: credential.map_or(0, |c| c.revision),
            excluded: excluded.is_some(),
        })
    }
}
