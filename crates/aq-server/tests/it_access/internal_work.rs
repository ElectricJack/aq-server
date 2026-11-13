//! aq-internal-work queue (spec 0012 §3, §4): blocking work first, one claim
//! per task, failover across the candidate list, and late runners ignored.
use aq_internal_work::{
    Affected, AnswerShape, ApiBackend, ApiEndpoint, Candidate, Claimed, Outcome, Purpose, Reason,
    Request, Usage, Way, store, validate,
};
use aq_server::test_support::TestDb;
use serde_json::json;
use sqlx::PgPool;

const NOW: i64 = 1_000_000;

async fn migrated(db: &TestDb) -> PgPool {
    db.store().await;
    db.pool().await
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
    }
}

fn api(provider: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: format!("{provider}-model"),
        reasoning: "medium".into(),
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

#[tokio::test]
async fn blocking_tasks_are_claimed_before_older_background_tasks() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let background = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let blocking = store::submit(&pool, &request(true), &[api("openai")], NOW + 10)
        .await
        .unwrap();
    let first = store::claim(&pool, Way::Api, &[], "runner-a", NOW + 20)
        .await
        .unwrap()
        .expect("a queued task");
    assert_eq!(first.id, blocking);
    assert_eq!(first.candidate_index, 0);
    assert_eq!(first.deadline_at_ms, NOW + 10 + 60_000);
    let second = store::claim(&pool, Way::Api, &[], "runner-a", NOW + 30)
        .await
        .unwrap()
        .expect("the background task");
    assert_eq!(second.id, background);
    let row = store::get(&pool, blocking).await.unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.runner.as_deref(), Some("runner-a"));
    assert_eq!(row.claimed_at_ms, Some(NOW + 20));
    assert!(
        store::claim(&pool, Way::Api, &[], "runner-a", NOW + 40)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_task_is_claimed_once_by_concurrent_runners() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let wide = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let id = store::submit(&wide, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let claims = (0..8).map(|i| {
        let pool = wide.clone();
        tokio::spawn(async move {
            store::claim(&pool, Way::Api, &[], &format!("runner-{i}"), NOW + 1)
                .await
                .unwrap()
                .map(|c| c.id)
        })
    });
    let mut won = Vec::new();
    for claim in claims.collect::<Vec<_>>() {
        if let Some(claimed) = claim.await.unwrap() {
            won.push(claimed);
        }
    }
    assert_eq!(won, [id]);
}

#[tokio::test]
async fn failed_candidate_requeues_on_the_next_and_records_the_attempt() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(false),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    let claimed = store::claim(&pool, Way::Api, &[], "r", NOW + 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.candidate.provider, "openai");
    store::fail_candidate(&pool, id, 0, Reason::ProviderError, "HTTP 500", NOW + 105)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.candidate_index, 1);
    assert_eq!(row.runner, None);
    assert_eq!(row.attempts.len(), 1);
    let attempt = &row.attempts[0];
    assert_eq!(
        (attempt.provider.as_str(), attempt.model.as_str()),
        ("openai", "openai-model")
    );
    assert_eq!(attempt.way, Way::Api);
    assert_eq!(attempt.reason, Some(Reason::ProviderError));
    assert_eq!(attempt.detail, "HTTP 500");
    assert_eq!(attempt.elapsed_ms, 100);
    assert!(row.outcome().is_none());

    let next = store::claim(&pool, Way::Api, &[], "r", NOW + 110)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((next.id, next.candidate_index), (id, 1));
    assert_eq!(next.candidate.provider, "anthropic");
    store::complete(
        &pool,
        id,
        1,
        &json!({"choice": "small"}),
        Some(&Usage {
            input_tokens: 10,
            output_tokens: 2,
        }),
        NOW + 150,
    )
    .await
    .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "answered");
    match row.outcome().unwrap() {
        Outcome::Answered {
            answer,
            provider,
            way,
            usage,
            attempts,
            ..
        } => {
            assert_eq!(answer, json!({"choice": "small"}));
            assert_eq!(provider, "anthropic");
            assert_eq!(way, Way::Api);
            assert_eq!(usage.unwrap().output_tokens, 2);
            assert_eq!(attempts[0].reason, Some(Reason::ProviderError));
        }
        other => panic!("expected an answer, got {other:?}"),
    }
}

#[tokio::test]
async fn last_candidate_failure_fails_the_task_with_all_attempts() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(
        &pool,
        id,
        0,
        Reason::ProviderUnavailable,
        "login expired",
        NOW + 1,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW + 2)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 1, Reason::InvalidOutput, "not JSON", NOW + 3)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.finished_at_ms, Some(NOW + 3));
    match row.outcome().unwrap() {
        Outcome::Failed {
            reason,
            detail,
            attempts,
        } => {
            assert_eq!(reason, Reason::InvalidOutput);
            assert_eq!(detail, "not JSON");
            assert_eq!(
                attempts.iter().map(|a| a.reason).collect::<Vec<_>>(),
                [
                    Some(Reason::ProviderUnavailable),
                    Some(Reason::InvalidOutput)
                ]
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    // BudgetExceeded fails at once even with a candidate left.
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::BudgetExceeded, "over budget", NOW + 1)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.reason),
        ("failed", Some(Reason::BudgetExceeded))
    );

    // A candidate left but the task deadline passed: fail with that reason.
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::DeadlineExceeded, "slow", NOW + 60_000)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.reason, row.candidate_index),
        ("failed", Some(Reason::DeadlineExceeded), 0)
    );
}

#[tokio::test]
async fn late_completion_from_a_superseded_candidate_is_ignored() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(false),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::DeadlineExceeded, "slow", NOW + 10)
        .await
        .unwrap();
    // The superseded runner answers late, while the row is queued again.
    store::complete(&pool, id, 0, &json!({"choice": "large"}), None, NOW + 11)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 0, Reason::ProviderError, "late", NOW + 11)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
    assert_eq!(row.answer, None);
    assert_eq!(row.attempts.len(), 1);

    // Still ignored once the next candidate is running.
    store::claim(&pool, Way::Api, &[], "r", NOW + 12)
        .await
        .unwrap()
        .unwrap();
    store::complete(&pool, id, 0, &json!({"choice": "large"}), None, NOW + 13)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.answer), ("running", None));

    store::complete(&pool, id, 1, &json!({"choice": "small"}), None, NOW + 14)
        .await
        .unwrap();
    // A finished task cannot be overwritten either.
    store::complete(&pool, id, 1, &json!({"choice": "large"}), None, NOW + 15)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 1, Reason::ProviderError, "late", NOW + 15)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "answered");
    assert_eq!(row.answer, Some(json!({"choice": "small"})));
    assert_eq!(row.finished_at_ms, Some(NOW + 14));
}

#[tokio::test]
async fn login_claim_skips_tasks_whose_candidate_is_api_or_another_harness() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    store::submit(&pool, &request(true), &[api("openai")], NOW)
        .await
        .unwrap();
    store::submit(
        &pool,
        &request(true),
        &[login("anthropic", "claude")],
        NOW + 1,
    )
    .await
    .unwrap();
    let codex = store::submit(&pool, &request(false), &[login("openai", "codex")], NOW + 2)
        .await
        .unwrap();
    let claimed = store::claim(&pool, Way::Login, &["codex"], "r", NOW + 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, codex);
    assert_eq!(claimed.candidate.harness.as_deref(), Some("codex"));
    assert!(
        store::claim(&pool, Way::Login, &["codex"], "r", NOW + 4)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store::claim(&pool, Way::Login, &[], "r", NOW + 4)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn submit_without_candidates_is_failed_no_candidate() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(&pool, &request(true), &[], NOW)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(Reason::NoCandidate));
    assert_eq!(row.finished_at_ms, Some(NOW));
    assert_eq!(row.workspace_kind, "none");
    assert_eq!(row.escalation, "none");
    assert_eq!(
        (row.affected_kind.as_str(), row.affected_id.as_str()),
        ("task", "T-1")
    );
    assert!(matches!(
        row.outcome(),
        Some(Outcome::Failed {
            reason: Reason::NoCandidate,
            ..
        })
    ));
    assert!(
        store::claim(&pool, Way::Api, &[], "r", NOW)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_bounds_are_enforced() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    assert!(request(false).validate().is_ok());
    let zero_deadline = Request {
        deadline_ms: 0,
        ..request(false)
    };
    assert!(zero_deadline.validate().is_err());
    assert!(
        store::submit(&pool, &zero_deadline, &[api("openai")], NOW)
            .await
            .is_err()
    );
    let big_input = Request {
        input: "x".repeat(2 * 1024 * 1024),
        ..request(false)
    };
    assert!(
        store::submit(&pool, &big_input, &[api("openai")], NOW)
            .await
            .is_err()
    );
    for bad in [
        Request {
            deadline_ms: 900_001,
            ..request(false)
        },
        Request {
            instructions: "x".repeat(256 * 1024 + 1),
            ..request(false)
        },
        Request {
            max_output_bytes: 0,
            ..request(false)
        },
        Request {
            max_output_bytes: 1_048_577,
            ..request(false)
        },
    ] {
        assert!(bad.validate().is_err());
    }
    let mut unknown = serde_json::to_value(request(false)).unwrap();
    unknown["purpose"] = json!("routing_clasification");
    assert!(serde_json::from_value::<Request>(unknown).is_err());
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(none, 0);
}

/// Candidate resolution (spec 0012 §5, §6): pure, so plain tests.
mod resolve {
    use aq_intelligence_config::{CredentialConfig, IntelligenceConfig, LoadedRegistry};
    use aq_internal_work::{Reason, Way, resolve};
    use aq_profiles::{ModelCandidate, ValidatedCatalog};
    use aq_selection::{
        ArtifactRef, AuthorizedCredential, Availability, AvailabilityObservation, CredentialKind,
        Digest, ObservationScope, ObservationSource, Text,
    };
    use uuid::Uuid;

    const NOW: u64 = 1_000_000;

    pub(super) fn candidate(id: &str, provider: &str, harnesses: &[&str]) -> ModelCandidate {
        ModelCandidate {
            id: id.into(),
            provider: provider.into(),
            model: format!("{provider}-model"),
            reasoning_configuration_reference: format!("{provider}-reasoning:1"),
            harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
        }
    }

    pub(super) fn catalog(profiles: Vec<(&str, Vec<ModelCandidate>)>) -> ValidatedCatalog {
        let base = aq_profiles::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../aq-profiles/examples/catalog"),
        )
        .unwrap();
        let instructions = base.artifact().instructions.clone();
        let mut catalog = base.artifact().catalog.clone();
        let template = catalog.intelligence[0].clone();
        let engineering = template.id.clone();
        catalog.intelligence = profiles
            .into_iter()
            .map(|(id, candidates)| {
                let mut profile = template.clone();
                profile.id = id.into();
                profile.candidates = candidates;
                profile
            })
            .collect();
        // Agents point at the example's own profile; keep it present.
        if !catalog.intelligence.iter().any(|p| p.id == engineering) {
            let mut keep = template;
            keep.id = engineering;
            catalog.intelligence.push(keep);
        }
        aq_profiles::validate(catalog, instructions).unwrap()
    }

    fn profile_ref(name: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(name).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(name.as_bytes()),
        }
    }

    pub(super) fn credential(name: &str, provider: &str, enabled: bool) -> AuthorizedCredential {
        AuthorizedCredential {
            profile: profile_ref(name),
            provider: provider.into(),
            account_reference: format!("{name}-account"),
            endpoint_reference: format!("{name}-endpoint"),
            revision: 1,
            enabled,
            provisionable: false,
            material_owner: None,
            allowed_backends: aq_selection::default_allowed_backends(),
        }
    }

    pub(super) fn registry(items: Vec<(AuthorizedCredential, CredentialKind)>) -> LoadedRegistry {
        IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: items
                .into_iter()
                .map(|(credential, kind)| CredentialConfig {
                    kind,
                    allowed_backends: aq_selection::default_allowed_backends(),
                    credential,
                    allowed_node_ids: vec![],
                    allow_enrolled_nodes: true,
                    secret_file: None,
                    delivery_mode: None,
                })
                .collect(),
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap()
    }

    fn observation(
        credential: &AuthorizedCredential,
        state: Availability,
        expires_at_ms: u64,
        cooldown_until_ms: u64,
    ) -> AvailabilityObservation {
        AvailabilityObservation {
            observation_id: Uuid::new_v4(),
            scope: ObservationScope {
                credential_revision: credential.revision,
                provider: credential.provider.clone(),
                credential_profile: credential.profile.clone(),
                account_reference: credential.account_reference.clone(),
                endpoint_reference: credential.endpoint_reference.clone(),
                model: None,
                node_id: None,
            },
            source: ObservationSource::Operator {
                principal_id: Uuid::from_u128(1),
            },
            sequence: 1,
            revision: 1,
            state,
            received_at_ms: NOW - 10,
            expires_at_ms,
            cooldown_until_ms,
        }
    }

    fn two_providers() -> (ValidatedCatalog, LoadedRegistry, AuthorizedCredential) {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("a", "anthropic", &["claude-code"]),
                candidate("o", "openai", &["codex"]),
            ],
        )]);
        let login = credential("anthropic-login", "anthropic", true);
        let registry = registry(vec![
            (login.clone(), CredentialKind::SubscriptionLogin),
            (
                credential("openai-key", "openai", true),
                CredentialKind::ApiKey,
            ),
        ]);
        (catalog, registry, login)
    }

    #[test]
    fn candidates_keep_profile_order_and_carry_the_way_of_their_credential() {
        let (catalog, registry, _) = two_providers();
        let resolved = resolve(&catalog, &registry, &[], "internal", NOW).unwrap();
        assert_eq!(resolved.profile_id, "internal");
        assert_eq!(resolved.profile_version, 1);
        let got: Vec<_> = resolved
            .candidates
            .iter()
            .map(|c| {
                (
                    c.provider.as_str(),
                    c.way,
                    c.harness.as_deref(),
                    c.credential_index,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("anthropic", Way::Login, Some("claude"), 0),
                ("openai", Way::Api, None, 1),
            ]
        );
        assert_eq!(resolved.candidates[0].reasoning, "anthropic-reasoning:1");
        assert_eq!(resolved.candidates[1].model, "openai-model");
    }

    #[test]
    fn quota_unavailable_login_is_skipped_so_the_next_provider_is_first() {
        let (catalog, registry, login) = two_providers();
        for state in [
            Availability::QuotaUnavailable,
            Availability::AuthenticationFailed,
            Availability::TemporaryUnavailable,
        ] {
            for cooldown in [0, NOW + 30_000] {
                let seen = [observation(&login, state, NOW + 60_000, cooldown)];
                let resolved = resolve(&catalog, &registry, &seen, "internal", NOW).unwrap();
                assert_eq!(
                    resolved.candidates.len(),
                    1,
                    "{state:?} cooldown {cooldown}"
                );
                assert_eq!(resolved.candidates[0].provider, "openai");
            }
            let seen = [observation(&login, state, NOW + 60_000, NOW + 30_000)];
            let resolved = resolve(&catalog, &registry, &seen, "internal", NOW).unwrap();
            assert_eq!(resolved.candidates.len(), 1, "{state:?}");
            assert_eq!(resolved.candidates[0].provider, "openai");
        }
        let available = [observation(
            &login,
            Availability::Available,
            NOW + 60_000,
            0,
        )];
        let resolved = resolve(&catalog, &registry, &available, "internal", NOW).unwrap();
        assert_eq!(resolved.candidates[0].provider, "anthropic");
    }

    fn lapsed_without_cooldown(login: &AuthorizedCredential) -> AvailabilityObservation {
        observation(login, Availability::QuotaUnavailable, NOW, 0)
    }

    #[test]
    fn expired_observation_does_not_skip() {
        let (catalog, registry, login) = two_providers();
        let lapsed = observation(&login, Availability::QuotaUnavailable, NOW, NOW + 30_000);
        for seen in [lapsed, lapsed_without_cooldown(&login)] {
            let resolved = resolve(&catalog, &registry, &[seen], "internal", NOW).unwrap();
            assert_eq!(resolved.candidates[0].provider, "anthropic");
        }
    }

    #[test]
    fn missing_profile_and_profile_without_usable_credentials_are_no_candidate() {
        let (catalog, usable, _) = two_providers();
        assert_eq!(
            resolve(&catalog, &usable, &[], "nope", NOW).unwrap_err(),
            Reason::NoCandidate
        );
        let disabled = registry(vec![
            (
                credential("anthropic-login", "anthropic", false),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("elsewhere", "other", true),
                CredentialKind::ApiKey,
            ),
        ]);
        assert_eq!(
            resolve(&catalog, &disabled, &[], "internal", NOW).unwrap_err(),
            Reason::NoCandidate
        );
    }

    #[test]
    fn login_candidate_whose_only_harness_is_gemini_is_dropped() {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("g", "google", &["gemini"]),
                candidate("o", "openai", &["codex", "gemini"]),
            ],
        )]);
        let registry = registry(vec![
            (
                credential("google-login", "google", true),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("openai-login", "openai", true),
                CredentialKind::SubscriptionLogin,
            ),
        ]);
        let resolved = resolve(&catalog, &registry, &[], "internal", NOW).unwrap();
        assert_eq!(resolved.candidates.len(), 1);
        assert_eq!(resolved.candidates[0].provider, "openai");
        assert_eq!(resolved.candidates[0].harness.as_deref(), Some("codex"));
    }

    #[test]
    fn claude_code_in_the_catalog_resolves_to_the_claude_harness() {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("a", "anthropic", &["gemini", "claude-code"]),
                candidate("c", "claude-direct", &["claude"]),
            ],
        )]);
        let registry = registry(vec![
            (
                credential("a-login", "anthropic", true),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("c-login", "claude-direct", true),
                CredentialKind::SubscriptionLogin,
            ),
        ]);
        let resolved = resolve(&catalog, &registry, &[], "internal", NOW).unwrap();
        let harnesses: Vec<_> = resolved
            .candidates
            .iter()
            .map(|c| c.harness.as_deref())
            .collect();
        assert_eq!(harnesses, vec![Some("claude"), Some("claude")]);
    }

    #[test]
    fn recent_observation_older_than_the_policy_age_without_cooldown_does_not_skip() {
        let (mut catalog_value, registry, login) =
            (two_providers().0, two_providers().1, two_providers().2);
        let mut raw = catalog_value.artifact().catalog.clone();
        raw.intelligence[0].selection_policy = Some(
            serde_json::from_value(serde_json::json!({
                "schema_version":1,"fallback_on":[],"allow_unknown_availability":true,
                "observation_max_age_ms":1000,"max_output_tokens":1000,"max_attempts":2,
                "max_lineage_elapsed_ms":60000,"required_capabilities":[],
                "budget":{"max_cost_microunits":null,"currency":null,"require_complete_accounting":false,
                    "require_hard_cost_enforcement":false,"unknown_cost":"deny"}
            }))
            .unwrap(),
        );
        let instructions = catalog_value.artifact().instructions.clone();
        catalog_value = aq_profiles::validate(raw, instructions).unwrap();
        let mut old = observation(&login, Availability::QuotaUnavailable, NOW + 60_000, 0);
        old.received_at_ms = NOW - 5_000;
        let resolved = resolve(&catalog_value, &registry, &[old.clone()], "internal", NOW).unwrap();
        assert_eq!(resolved.candidates[0].provider, "anthropic");
        old.received_at_ms = NOW - 500;
        let resolved = resolve(&catalog_value, &registry, &[old], "internal", NOW).unwrap();
        assert_eq!(resolved.candidates[0].provider, "openai");
    }
}

fn choice_shape() -> AnswerShape {
    AnswerShape::Choice {
        choices: vec!["small".into(), "large".into()],
        rationale_max_bytes: 16,
    }
}

#[test]
fn fenced_json_is_accepted_and_prose_is_invalid_output() {
    let shape = choice_shape();
    let fenced = "```json\n{\"choice\": \"small\", \"rationale\": \"ok\"}\n```";
    assert_eq!(
        validate::answer(&shape, fenced, 4_096).unwrap(),
        json!({"choice": "small", "rationale": "ok"})
    );
    let bare = "```\n{\"choice\": \"large\", \"rationale\": \"\"}\n```";
    assert!(validate::answer(&shape, bare, 4_096).is_ok());
    let err = validate::answer(&shape, "I think the task is small.", 4_096).unwrap_err();
    assert!(err.contains("not JSON"), "{err}");
    let err = validate::answer(&shape, "[1]", 4_096).unwrap_err();
    assert!(err.contains("not a JSON object"), "{err}");
    let prompt = validate::prompt(&request(false));
    assert!(prompt.starts_with("Classify the task.\n\nTask: fix the login page\n\n"));
    assert!(prompt.ends_with(
        "Answer with one JSON object and nothing else: {\"choice\": one of [\"small\",\"large\"], \"rationale\": string}"
    ));
}

#[test]
fn choice_outside_the_list_and_oversized_rationale_are_invalid() {
    let shape = choice_shape();
    let err =
        validate::answer(&shape, r#"{"choice": "huge", "rationale": "x"}"#, 4_096).unwrap_err();
    assert!(err.contains("huge") && err.contains("choices"), "{err}");
    let long = format!(
        r#"{{"choice": "small", "rationale": "{}"}}"#,
        "x".repeat(17)
    );
    let err = validate::answer(&shape, &long, 4_096).unwrap_err();
    assert!(err.contains("rationale") && err.contains("16"), "{err}");
    let err = validate::answer(&shape, r#"{"choice": "small"}"#, 4_096).unwrap_err();
    assert!(err.contains("rationale"), "{err}");
}

#[test]
fn json_answer_is_checked_against_the_schema() {
    let shape = AnswerShape::Json {
        schema: json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }),
    };
    assert_eq!(
        validate::answer(&shape, r#"{"n": 3}"#, 4_096).unwrap(),
        json!({"n": 3})
    );
    assert!(validate::answer(&shape, r#"{"n": "three"}"#, 4_096).is_err());
    assert!(validate::answer(&shape, "{}", 4_096).is_err());
    let secret_text = "s".repeat(1_000);
    let wrong = format!(r#"{{"n": "{secret_text}"}}"#);
    let err = validate::answer(&shape, &wrong, 4_096).unwrap_err();
    assert!(
        err.len() <= 220 && !err.contains(&secret_text),
        "{}",
        err.len()
    );
    let bad_schema = AnswerShape::Json {
        schema: json!({"type": 12}),
    };
    assert!(validate::answer(&bad_schema, "{}", 4_096).is_err());
}

#[test]
fn oversized_output_is_invalid_and_the_error_does_not_echo_it() {
    let shape = choice_shape();
    let big = format!("zzz-marker-{}", "z".repeat(5_000));
    let err = validate::answer(&shape, &big, 100).unwrap_err();
    assert!(err.contains("5011") && err.contains("100"), "{err}");
    assert!(!err.contains("zzz"), "{err}");
    let err = validate::answer(&shape, &format!("{{{}", "y".repeat(5_000)), 9_999).unwrap_err();
    assert!(err.len() <= 220, "{}", err.len());
}

mod fake {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Seen {
        pub authorization: Vec<String>,
        pub bodies: Vec<Value>,
    }

    /// Answers every request with `status` and `reply`; returns its endpoint.
    pub async fn serve(status: u16, reply: Value) -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let app = Router::new()
            .route(
                "/v1/responses",
                post(
                    move |State(seen): State<Arc<Mutex<Seen>>>,
                          headers: HeaderMap,
                          axum::Json(body): axum::Json<Value>| {
                        let reply = reply.clone();
                        async move {
                            let mut seen = seen.lock().unwrap();
                            seen.authorization.push(
                                headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or_default()
                                    .to_owned(),
                            );
                            seen.bodies.push(body);
                            (
                                axum::http::StatusCode::from_u16(status).unwrap(),
                                axum::Json(reply),
                            )
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1/responses"), seen)
    }

    pub fn completed(text: &str) -> Value {
        json!({
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 12, "output_tokens": 5}
        })
    }
}

fn secret_file(dir: &std::path::Path, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("key");
    std::fs::write(&path, "sk-test-secret-value\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn backend(endpoint: &str, secret: std::path::PathBuf) -> ApiBackend {
    ApiBackend::new(vec![ApiEndpoint {
        provider: "openai".into(),
        endpoint: endpoint.into(),
        secret_file: secret,
    }])
    .unwrap()
}

fn claimed(provider: &str) -> Claimed {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Claimed {
        id: uuid::Uuid::new_v4(),
        request: request(false),
        candidate: api(provider),
        candidate_index: 0,
        deadline_at_ms: now + 30_000,
    }
}

#[tokio::test]
async fn api_backend_returns_text_and_usage_and_never_stores_the_secret() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let text = r#"{"choice": "small", "rationale": "ok"}"#;
    let (endpoint, seen) = fake::serve(200, fake::completed(text)).await;
    let api_backend = backend(&endpoint, secret_file(dir.path(), 0o600));

    let id = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let held = store::claim(&pool, Way::Api, &[], "r", NOW + 5)
        .await
        .unwrap()
        .unwrap();
    // The claimed deadline is in the past relative to the wall clock.
    let past = api_backend.run(&held).await.unwrap_err();
    assert_eq!(past.0, Reason::DeadlineExceeded);

    let live = Claimed {
        deadline_at_ms: held.deadline_at_ms + 4_000_000_000_000,
        ..held
    };
    let (raw, usage) = api_backend.run(&live).await.unwrap();
    assert_eq!(raw, text);
    let usage = usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.authorization, ["Bearer sk-test-secret-value"]);
        let body = &seen.bodies[0];
        assert_eq!(body["model"], "openai-model");
        assert_eq!(body["store"], false);
        assert!(
            body["input"]
                .as_str()
                .unwrap()
                .contains("Task: fix the login page")
        );
    }
    let answer = validate::answer(&choice_shape(), &raw, 4_096).unwrap();
    store::complete(&pool, id, 0, &answer, Some(&usage), NOW + 50)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    let stored = format!("{row:?}{:?}", row.outcome());
    assert!(!stored.contains("sk-test-secret-value"), "{stored}");

    // Error strings carry no key either.
    let bad = backend(&endpoint, dir.path().join("missing"));
    let err = bad.run(&claimed("openai")).await.unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        !err.1.contains("sk-test") && !err.1.contains("missing"),
        "{}",
        err.1
    );
    let none = api_backend.run(&claimed("anthropic")).await.unwrap_err();
    assert_eq!(
        none,
        (
            Reason::ProviderUnavailable,
            "no api endpoint configured for anthropic".to_string()
        )
    );
}

#[tokio::test]
async fn api_429_and_500_are_unavailable_and_400_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let secret = secret_file(dir.path(), 0o600);
    for (status, reason) in [
        (429, Reason::ProviderUnavailable),
        (408, Reason::ProviderUnavailable),
        (500, Reason::ProviderUnavailable),
        (503, Reason::ProviderUnavailable),
        (400, Reason::ProviderError),
        (401, Reason::ProviderError),
    ] {
        let (endpoint, _) = fake::serve(status, json!({"error": "sk-test-secret-value"})).await;
        let err = backend(&endpoint, secret.clone())
            .run(&claimed("openai"))
            .await
            .unwrap_err();
        assert_eq!(err.0, reason, "HTTP {status}");
        assert!(err.1.contains(&status.to_string()), "{}", err.1);
        assert!(!err.1.contains("sk-test"), "{}", err.1);
    }
    // Nothing listens on this port: a transport error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/responses", closed.local_addr().unwrap());
    drop(closed);
    let err = backend(&endpoint, secret)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    // An incomplete response is a provider error.
    let (endpoint, _) = fake::serve(200, json!({"status": "incomplete"})).await;
    let dir2 = tempfile::tempdir().unwrap();
    let err = backend(&endpoint, secret_file(dir2.path(), 0o600))
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderError);
}

#[tokio::test]
async fn secret_file_that_is_group_readable_or_a_symlink_is_refused() {
    let (endpoint, seen) = fake::serve(200, fake::completed("{}")).await;
    let dir = tempfile::tempdir().unwrap();
    let group = secret_file(dir.path(), 0o640);
    let err = backend(&endpoint, group)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(err.1.contains("unusable"), "{}", err.1);

    let dir = tempfile::tempdir().unwrap();
    let real = secret_file(dir.path(), 0o600);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = backend(&endpoint, link)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        seen.lock().unwrap().authorization.is_empty(),
        "nothing was sent"
    );

    // A relative path and a plain-http remote endpoint are refused up front.
    for (url, file) in [
        (endpoint.as_str(), std::path::PathBuf::from("key")),
        ("http://example.com/v1/responses", real.clone()),
    ] {
        assert!(
            ApiBackend::new(vec![ApiEndpoint {
                provider: "openai".into(),
                endpoint: url.into(),
                secret_file: file,
            }])
            .is_err()
        );
    }
}

/// The service (spec 0012 §3, §4, §8 step 1): Settings pick the profile, the
/// in-server loop runs API candidates in profile order, and the sweeper ends
/// tasks whose runner or deadline is gone.
mod service {
    use super::{fake, migrated, request, resolve, secret_file};
    use aq_intelligence_config::LoadedRegistry;
    use aq_internal_work::{ApiEndpoint, InternalWorkConfig, Outcome, Reason, Request, Way, store};
    use aq_selection::CredentialKind;
    use aq_server::{InternalWork, test_support::TestDb};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::watch;
    use uuid::Uuid;

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn user(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'owner',now())")
            .bind(id)
            .bind(format!("{id}@localhost"))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    /// Activates a catalog whose profile `internal` lists openai, then
    /// anthropic, and makes it the internal work default.
    async fn configure(pool: &PgPool) {
        let catalog = resolve::catalog(vec![(
            "internal",
            vec![
                resolve::candidate("o", "openai", &["codex"]),
                resolve::candidate("a", "anthropic", &["claude-code"]),
            ],
        )]);
        sqlx::query(
            "INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms)
             VALUES($1,$2,$3,1)",
        )
        .bind(catalog.digest())
        .bind(catalog.canonical_bytes().unwrap())
        .bind(user(pool).await)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
            .bind(catalog.digest())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(json!({"default_profile_id": "internal", "overrides": {}}))
            .execute(pool)
            .await
            .unwrap();
    }

    fn registry(anthropic: CredentialKind) -> Arc<LoadedRegistry> {
        Arc::new(resolve::registry(vec![
            (
                resolve::credential("openai-key", "openai", true),
                CredentialKind::ApiKey,
            ),
            (
                resolve::credential("anthropic-key", "anthropic", true),
                anthropic,
            ),
        ]))
    }

    struct Fixture {
        _db: TestDb,
        _dir: tempfile::TempDir,
        pool: PgPool,
        work: InternalWork,
        openai: Arc<std::sync::Mutex<fake::Seen>>,
        anthropic: Arc<std::sync::Mutex<fake::Seen>>,
    }

    /// A configured service whose providers answer `openai` and `anthropic`.
    async fn fixture(openai: &str, anthropic: &str, kind: CredentialKind) -> Fixture {
        let db = TestDb::create().await;
        let pool = migrated(&db).await;
        configure(&pool).await;
        let dir = tempfile::tempdir().unwrap();
        let secret = secret_file(dir.path(), 0o600);
        let (openai_url, openai) = fake::serve(200, fake::completed(openai)).await;
        let (anthropic_url, anthropic) = fake::serve(200, fake::completed(anthropic)).await;
        let config = InternalWorkConfig {
            api: [("openai", openai_url), ("anthropic", anthropic_url)]
                .into_iter()
                .map(|(provider, endpoint)| ApiEndpoint {
                    provider: provider.into(),
                    endpoint,
                    secret_file: secret.clone(),
                })
                .collect(),
            poll_ms: 20,
            enabled: true,
        };
        let work = InternalWork::new(
            pool.clone(),
            aq_server::system_clock(),
            Some(&config),
            Some(registry(kind)),
        )
        .unwrap();
        Fixture {
            _db: db,
            _dir: dir,
            pool,
            work,
            openai,
            anthropic,
        }
    }

    const SMALL: &str = r#"{"choice": "small", "rationale": "one page"}"#;
    const LARGE: &str = r#"{"choice": "large", "rationale": "many pages"}"#;

    /// Runs the in-server loop until the returned sender is dropped.
    fn start(work: &InternalWork) -> watch::Sender<bool> {
        let (stop, rx) = watch::channel(false);
        tokio::spawn(work.clone().serve(rx));
        stop
    }

    async fn wait(work: &InternalWork, id: Uuid) -> Outcome {
        tokio::time::timeout(Duration::from_secs(20), work.wait(id))
            .await
            .expect("the task finishes")
            .unwrap()
    }

    fn answered(outcome: Outcome) -> (Value, String, Vec<aq_internal_work::Attempt>) {
        match outcome {
            Outcome::Answered {
                answer,
                provider,
                way,
                attempts,
                ..
            } => {
                assert_eq!(way, Way::Api);
                (answer, provider, attempts)
            }
            failed => panic!("expected an answer, got {failed:?}"),
        }
    }

    #[tokio::test]
    async fn run_returns_the_validated_answer_from_the_first_candidate() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let _stop = start(&f.work);
        let outcome = tokio::time::timeout(Duration::from_secs(20), f.work.run(request(true)))
            .await
            .expect("run returns")
            .unwrap();
        let usage = match &outcome {
            Outcome::Answered { usage, .. } => usage.clone().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
        let (answer, provider, attempts) = answered(outcome);
        assert_eq!(answer, json!({"choice": "small", "rationale": "one page"}));
        assert_eq!(provider, "openai");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].reason, None);
        assert_eq!(f.openai.lock().unwrap().bodies.len(), 1);
        assert!(f.anthropic.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn invalid_output_from_the_first_candidate_falls_to_the_second() {
        let f = fixture("I would say small.", LARGE, CredentialKind::ApiKey).await;
        let _stop = start(&f.work);
        let (answer, provider, attempts) = answered(f.work.run(request(false)).await.unwrap());
        assert_eq!(answer["choice"], "large");
        assert_eq!(provider, "anthropic");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "openai");
        assert_eq!(attempts[0].reason, Some(Reason::InvalidOutput));
        assert!(
            attempts[0].detail.contains("not JSON"),
            "{}",
            attempts[0].detail
        );
        assert_eq!(attempts[1].provider, "anthropic");
        assert_eq!(attempts[1].reason, None);
    }

    #[tokio::test]
    async fn no_configured_profile_fails_no_candidate_naming_the_setting() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(json!({"default_profile_id": null, "overrides": {}}))
            .execute(&f.pool)
            .await
            .unwrap();
        let id = f.work.submit(request(true)).await.unwrap();
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason,
                detail,
                attempts,
            } => {
                assert_eq!(reason, Reason::NoCandidate);
                assert_eq!(detail, "no internal work profile configured");
                assert!(attempts.is_empty());
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        // An explicit override still runs without the setting.
        let _stop = start(&f.work);
        let overridden = Request {
            profile_override: Some("internal".into()),
            ..request(true)
        };
        let (_, provider, _) = answered(f.work.run(overridden).await.unwrap());
        assert_eq!(provider, "openai");
    }

    #[tokio::test]
    async fn a_running_task_whose_runner_vanished_is_swept_and_retried_on_the_next_candidate() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let id = f.work.submit(request(false)).await.unwrap();
        let held = store::claim(&f.pool, Way::Api, &[], "vanished", now_ms())
            .await
            .unwrap()
            .expect("the queued task");
        assert_eq!((held.id, held.candidate.provider.as_str()), (id, "openai"));
        // Within its 120 s budget the runner is left alone.
        assert_eq!(store::sweep(&f.pool, now_ms()).await.unwrap(), 0);
        assert_eq!(
            store::get(&f.pool, id).await.unwrap().unwrap().status,
            "running"
        );
        sqlx::query("UPDATE internal_tasks SET claimed_at_ms=$2 WHERE id=$1")
            .bind(id)
            .bind(now_ms() - 126_000)
            .execute(&f.pool)
            .await
            .unwrap();
        let _stop = start(&f.work);
        let (answer, provider, attempts) = answered(wait(&f.work, id).await);
        assert_eq!(
            (answer["choice"].as_str(), provider.as_str()),
            (Some("large"), "anthropic")
        );
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "openai");
        assert_eq!(attempts[0].reason, Some(Reason::DeadlineExceeded));
        assert_eq!(attempts[0].detail, "runner did not report");
        assert!(f.openai.lock().unwrap().bodies.is_empty());
        // The vanished runner reporting late changes nothing.
        store::complete(&f.pool, id, 0, &json!({"choice": "small"}), None, now_ms())
            .await
            .unwrap();
        let (answer, provider, _) = answered(wait(&f.work, id).await);
        assert_eq!(
            (answer["choice"].as_str(), provider.as_str()),
            (Some("large"), "anthropic")
        );
    }

    #[tokio::test]
    async fn a_queued_task_past_its_deadline_fails_deadline_exceeded() {
        // Only anthropic resolves, by login: nothing on the server claims it yet.
        let f = fixture(SMALL, LARGE, CredentialKind::SubscriptionLogin).await;
        configure_login_only(&f.pool).await;
        let login_only = Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        let id = f.work.submit(login_only).await.unwrap();
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert_eq!(row.status, "queued");
        assert_eq!(row.candidates[0].way, Way::Login);
        let _stop = start(&f.work);
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason, attempts, ..
            } => {
                assert_eq!(reason, Reason::DeadlineExceeded);
                assert!(attempts.is_empty());
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert!(row.finished_at_ms.unwrap() >= row.deadline_at_ms);
    }

    fn failed_deadline(outcome: Outcome) {
        match outcome {
            Outcome::Failed { reason, .. } => assert_eq!(reason, Reason::DeadlineExceeded),
            answered => panic!("expected a failure, got {answered:?}"),
        }
    }

    #[tokio::test]
    async fn with_internal_work_disabled_the_loop_claims_nothing_but_still_fails_past_deadline() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let config = InternalWorkConfig {
            api: vec![],
            poll_ms: 20,
            enabled: false,
        };
        let registry = Some(registry(CredentialKind::ApiKey));
        let clock = aq_server::system_clock();
        let disabled = InternalWork::new(f.pool.clone(), clock, Some(&config), registry).unwrap();
        let _stop = start(&disabled);
        let short = Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        let started = std::time::Instant::now();
        let id = disabled.submit(short).await.unwrap();
        failed_deadline(wait(&disabled, id).await);
        // The loop's sweep ends it, well before wait's own sweep would.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert!(row.attempts.is_empty());
        assert!(f.openai.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn wait_without_a_running_loop_still_fails_a_login_task_past_its_deadline() {
        let f = fixture(SMALL, LARGE, CredentialKind::SubscriptionLogin).await;
        configure_login_only(&f.pool).await;
        let short = || Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        // A login host claims one task and dies without reporting.
        let running = f.work.submit(short()).await.unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let held = store::claim(&f.pool, Way::Login, &["claude"], "dead-host", now)
            .await
            .unwrap()
            .expect("the login task");
        assert_eq!(held.id, running);
        // Nothing claims the other one.
        let queued = f.work.submit(short()).await.unwrap();
        let (queued, running) = tokio::join!(wait(&f.work, queued), wait(&f.work, running));
        failed_deadline(queued);
        match running {
            Outcome::Failed {
                reason, attempts, ..
            } => {
                assert_eq!(reason, Reason::DeadlineExceeded);
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].detail, "runner did not report");
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
    }

    /// A second active catalog whose profile `internal` lists only anthropic.
    async fn configure_login_only(pool: &PgPool) {
        let catalog = resolve::catalog(vec![(
            "internal",
            vec![resolve::candidate("a", "anthropic", &["claude-code"])],
        )]);
        sqlx::query(
            "INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms)
             VALUES($1,$2,$3,1)",
        )
        .bind(catalog.digest())
        .bind(catalog.canonical_bytes().unwrap())
        .bind(user(pool).await)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
            .bind(catalog.digest())
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn internal_work_runs_while_swarm_availability_is_off_and_adds_no_swarm_admission_rows() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let owner = user(&f.pool).await;
        let mut conn = f.pool.acquire().await.unwrap();
        let off = aq_machine_control::set_owner_availability(
            &mut conn,
            false,
            1,
            owner,
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(off.state, aq_machine_control::Availability::Off);
        drop(conn);
        let _stop = start(&f.work);
        let (_, provider, _) = answered(f.work.run(request(true)).await.unwrap());
        assert_eq!(provider, "openai");
        for table in ["swarm_supervisor_admissions", "swarm_waiting"] {
            let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&f.pool)
                .await
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }
}
