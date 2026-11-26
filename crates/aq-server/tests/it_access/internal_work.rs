//! aq-internal-work queue (spec 0012 §3, §4): blocking work first, one claim
//! per task, failover across the candidate list, and late runners ignored.
use aq_internal_work::{
    Affected, AnswerShape, ApiBackend, ApiEndpoint, Candidate, Claimed, Outcome, Purpose, Reason,
    Request, SWEEP_GRACE_MS, Usage, Way, agent_candidate_budget_ms, candidate_budget_ms, store,
    validate,
};
use aq_server::test_support::TestDb;
use serde_json::json;
use sqlx::PgPool;

const NOW: i64 = 1_000_000;

async fn migrated(db: &TestDb) -> PgPool {
    db.store().await;
    db.pool().await
}

fn choice_shape() -> AnswerShape {
    AnswerShape::Choice {
        choices: vec!["small".into(), "large".into()],
        rationale_max_bytes: 16,
    }
}

fn request(blocking: bool) -> Request {
    Request {
        purpose: Purpose::RoutingClassification,
        instructions: "Classify the task.".into(),
        input: "Task: fix the login page".into(),
        answer: AnswerShape::Choice {
            choices: vec!["small".into(), "large".into()],
            rationale_max_bytes: 512,
        },
        blocking,
        affected: Affected {
            kind: "task".into(),
            id: "T-1".into(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: true,
        profile_override: None,
        agent: None,
    }
}

fn api(provider: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: format!("{provider}-model"),
        reasoning: Some("medium".into()),
        harness: None,
        way: Way::Api,
        credential_index: 0,
    }
}

fn login(provider: &str, harness: &str) -> Candidate {
    Candidate {
        harness: Some(harness.into()),
        way: Way::Login,
        ..api(provider)
    }
}

mod failover;
mod queue;

mod submission;

mod sweeps;

/// A request for agent work (spec 0013 §4.2): files in a workspace, an
/// outcome file as the answer, up to 30 minutes.
fn agent_request(blocking: bool) -> Request {
    Request {
        purpose: Purpose::PlaybookCompilation,
        answer: AnswerShape::Json {
            schema: json!({"type": "object", "required": ["kind"]}),
        },
        deadline_ms: 1_800_000,
        max_output_bytes: 1_048_576,
        agent: Some(aq_internal_work::AgentWork {
            files: [("doc.md".to_string(), "# Doc".to_string())].into(),
            max_turns: 200,
        }),
        ..request(blocking)
    }
}

mod agent_queue;

/// Candidate resolution (spec 0012 §5, §6): pure, so plain tests.
mod resolve;

mod validation;

use crate::common::fake_api as fake;

fn secret_file(dir: &std::path::Path, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("key");
    std::fs::write(&path, "sk-test-secret-value\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

mod api_backend;

/// The service (spec 0012 §3, §4, §8 step 1): Settings pick the profile, the
/// in-server loop runs API candidates in profile order, and the sweeper ends
/// tasks whose runner or deadline is gone.
mod service;

/// The owner-only runs routes (spec 0012 §7) through the real admin router.
mod runs_api;

/// The failure path (spec 0012 §8): a supervisor first, the owner only when
/// the supervisor cannot fix it.
mod escalation;
