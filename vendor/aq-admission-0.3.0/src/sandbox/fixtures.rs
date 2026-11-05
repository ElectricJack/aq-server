//! Shared, published conformance vectors. Expected outcomes are literal fixture
//! data; these helpers exercise the same validators used by both consumers.
use super::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub name: String,
    pub case: FixtureCase,
    pub expected: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureCase {
    PhaseHandoff {
        authority: PhaseExecutionAuthority,
        permit: DelegatedPermit,
        now_ms: u64,
    },
    RunnerRenewal {
        request: Box<RunnerRenewalRequest>,
        response: RunnerRenewalResponse,
        context: PermitContext,
        nonce: Uuid,
        now_ms: u64,
    },
    Permit {
        permit: DelegatedPermit,
        context: PermitContext,
        operation: RunnerOperation,
        now_ms: u64,
    },
    Readoption {
        request: ReadoptionRequest,
        response: ReadoptionResponse,
        previous_permits: Vec<DelegatedPermit>,
        now_ms: u64,
    },
    ReadoptionReplay {
        original_request: ReadoptionRequest,
        original_response: ReadoptionResponse,
        request: ReadoptionRequest,
        response: ReadoptionResponse,
        previous_permits: Vec<DelegatedPermit>,
        now_ms: u64,
    },
    Compatibility {
        required: Vec<String>,
        supported: Vec<String>,
    },
    Registration {
        registration: Registration,
    },
    PreparationPermit {
        authority: PreparationAuthority,
        permit: DelegatedPermit,
        operation: RunnerOperation,
        now_ms: u64,
    },
    PermitIssuance {
        request: PermitIssuanceRequest,
        response: Box<PermitIssuanceResponse>,
        now_ms: u64,
    },
    PermitIssuanceReplay {
        original_request: PermitIssuanceRequest,
        original_response: Box<PermitIssuanceResponse>,
        request: PermitIssuanceRequest,
        response: Box<PermitIssuanceResponse>,
        now_ms: u64,
    },
    PermitUpgrade {
        request: PermitUpgradeRequest,
        response: Box<PermitUpgradeResponse>,
        preparation: PreparationAuthority,
        preparation_permit: DelegatedPermit,
        now_ms: u64,
    },
    PermitUpgradeReplay {
        original_request: PermitUpgradeRequest,
        original_response: Box<PermitUpgradeResponse>,
        request: PermitUpgradeRequest,
        response: Box<PermitUpgradeResponse>,
        preparation: PreparationAuthority,
        preparation_permit: DelegatedPermit,
        now_ms: u64,
    },
    RegistrationEnrollment {
        registration: Box<Registration>,
        response: RegistrationResponse,
    },
    RegistrationReplay {
        original_registration: Box<Registration>,
        original_response: RegistrationResponse,
        registration: Box<Registration>,
        response: RegistrationResponse,
    },
}
pub fn validate_fixture(fixture: &Fixture) -> String {
    let result = match &fixture.case {
        FixtureCase::PhaseHandoff {
            authority,
            permit,
            now_ms,
        } => authority.validate_handoff(permit, *now_ms),
        FixtureCase::RunnerRenewal {
            request,
            response,
            context,
            nonce,
            now_ms,
        } => response.validate_for(request, context, *nonce, *now_ms),
        FixtureCase::Permit {
            permit,
            context,
            operation,
            now_ms,
        } => permit.validate_for(context, *operation, *now_ms),
        FixtureCase::Readoption {
            request,
            response,
            previous_permits,
            now_ms,
        } => response.validate_for(request, previous_permits, *now_ms),
        FixtureCase::ReadoptionReplay {
            original_request,
            original_response,
            request,
            response,
            previous_permits,
            now_ms,
        } => validate_replay(
            original_request,
            original_response,
            request,
            response,
            previous_permits,
            *now_ms,
        ),
        FixtureCase::Compatibility {
            required,
            supported,
        } => negotiate(required, supported).and_then(|reply| match reply {
            CompatibilityReply::Compatible => Ok(()),
            CompatibilityReply::UnsupportedRequiredCapability { .. } => {
                Err(ContractError::UnsupportedRequiredCapability)
            }
        }),
        FixtureCase::Registration { registration } => registration.validate(),
        FixtureCase::PreparationPermit {
            authority,
            permit,
            operation,
            now_ms,
        } => authority.validate_permit(permit, *operation, *now_ms),
        FixtureCase::PermitIssuance {
            request,
            response,
            now_ms,
        } => response.validate_for(request, *now_ms),
        FixtureCase::PermitIssuanceReplay {
            original_request,
            original_response,
            request,
            response,
            now_ms,
        } => validate_issuance_replay(
            original_request,
            original_response,
            request,
            response,
            *now_ms,
        ),
        FixtureCase::PermitUpgrade {
            request,
            response,
            preparation,
            preparation_permit,
            now_ms,
        } => request
            .validate_for(preparation, preparation_permit, *now_ms)
            .and_then(|()| {
                response.validate_for(request, preparation, preparation_permit, *now_ms)
            }),
        FixtureCase::PermitUpgradeReplay {
            original_request,
            original_response,
            request,
            response,
            preparation,
            preparation_permit,
            now_ms,
        } => validate_upgrade_replay(
            original_request,
            original_response,
            request,
            response,
            preparation,
            preparation_permit,
            *now_ms,
        ),
        FixtureCase::RegistrationEnrollment {
            registration,
            response,
        } => response.validate_for(registration),
        FixtureCase::RegistrationReplay {
            original_registration,
            original_response,
            registration,
            response,
        } => validate_registration_replay(
            original_registration,
            original_response,
            registration,
            response,
        ),
    };
    match result {
        Ok(()) => "accepted".into(),
        Err(code) => serde_json::to_value(code)
            .expect("error code")
            .as_str()
            .expect("string code")
            .into(),
    }
}
