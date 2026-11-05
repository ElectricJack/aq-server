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
