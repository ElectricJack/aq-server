use super::{fake, login, migrated, request, resolve, secret_file};
use aq_intelligence_config::LoadedRegistry;
use aq_internal_work::{
    ApiEndpoint, InternalWorkConfig, Outcome, Reason, Request, SWEEP_GRACE_MS, Way,
    candidate_budget_ms, store,
};
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

/// Saves the level `internal` with `rows` as the owner would.
async fn save_level(pool: &PgPool, rows: &[resolve::Row<'_>]) {
    let document = resolve::level("internal", rows, &[]);
    let kind = aq_profiles::roles::Kind::Intelligence;
    crate::common::roles::save(pool, user(pool).await, kind, "internal", &document).await;
}

/// Adds the level `internal`, listing openai, then anthropic, to the
/// active roles and profiles and makes it the internal work default.
async fn configure(pool: &PgPool) {
    let rows: [resolve::Row; 2] = [
        ("openai", "low", &["codex"]),
        ("anthropic", "high", &["claude"]),
    ];
    save_level(pool, &rows).await;
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

/// Agent work needs a harness with tools: a profile of API candidates
/// only is refused at submission, saying so, and no provider is called;
/// a profile with a login keeps only its logged-in candidates.
#[tokio::test]
async fn agent_work_whose_profile_has_no_logged_in_harness_is_refused_at_submission() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let _stop = start(&f.work);
    let id = f.work.submit(super::agent_request(true)).await.unwrap();
    match wait(&f.work, id).await {
        Outcome::Failed {
            reason,
            detail,
            attempts,
        } => {
            assert_eq!(reason, Reason::NoCandidate);
            assert_eq!(
                detail,
                "agent work needs a logged-in harness, and intelligence profile \"internal\" lists none"
            );
            assert!(attempts.is_empty());
        }
        answered => panic!("expected a failure, got {answered:?}"),
    }
    assert!(f.openai.lock().unwrap().bodies.is_empty());
    assert!(f.anthropic.lock().unwrap().bodies.is_empty());
    let f = fixture(SMALL, LARGE, CredentialKind::SubscriptionLogin).await;
    let id = f.work.submit(super::agent_request(true)).await.unwrap();
    let row = store::get(&f.pool, id).await.unwrap().unwrap();
    let ways: Vec<Way> = row.candidates.iter().map(|c| c.way).collect();
    assert_eq!(ways, [Way::Login]);
    assert_eq!(row.status, "queued");
}

/// Agent work stored with an API candidate (not through the service) is
/// failed over by the API loop without calling the provider.
#[tokio::test]
async fn agent_work_is_never_sent_to_an_api_candidate() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let _stop = start(&f.work);
    let candidates = [super::api("openai"), login("anthropic", "claude")];
    let id = store::submit(&f.pool, &super::agent_request(true), &candidates, now_ms())
        .await
        .unwrap();
    let moved = tokio::time::timeout(Duration::from_secs(10), async {
        while store::get(&f.pool, id)
            .await
            .unwrap()
            .unwrap()
            .candidate_index
            == 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(moved.is_ok(), "the API candidate was not failed over");
    let row = store::get(&f.pool, id).await.unwrap().unwrap();
    assert_eq!(row.attempts[0].reason, Some(Reason::ProviderUnavailable));
    assert_eq!(
        row.attempts[0].detail,
        aq_internal_work::service::AGENT_NEEDS_LOGIN
    );
    assert!(f.openai.lock().unwrap().bodies.is_empty());
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

/// A purpose whose stored level no longer exists runs on the default;
/// when the default's level is gone too the task fails naming it.
#[tokio::test]
async fn a_missing_level_falls_back_to_the_default_and_a_missing_default_is_named() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let _stop = start(&f.work);
    let set = |document: Value| {
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(document)
            .execute(&f.pool)
    };
    set(json!({"default_profile_id": "internal",
        "overrides": {"routing_classification": "gone"}}))
    .await
    .unwrap();
    let (_, provider, _) = answered(f.work.run(request(true)).await.unwrap());
    assert_eq!(provider, "openai");
    let asked = f.openai.lock().unwrap().bodies.len();

    set(json!({"default_profile_id": "gone", "overrides": {}}))
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
            assert!(detail.contains("\"gone\""), "{detail}");
            assert!(attempts.is_empty());
        }
        answered => panic!("expected a failure, got {answered:?}"),
    }
    // An override naming no level is no candidate, by name.
    let overridden = Request {
        profile_override: Some("nowhere".into()),
        ..request(true)
    };
    let id = f.work.submit(overridden).await.unwrap();
    match wait(&f.work, id).await {
        Outcome::Failed { reason, detail, .. } => {
            assert_eq!(reason, Reason::NoCandidate);
            assert!(detail.contains("\"nowhere\""), "{detail}");
        }
        answered => panic!("expected a failure, got {answered:?}"),
    }
    assert_eq!(f.openai.lock().unwrap().bodies.len(), asked);
}

#[tokio::test]
async fn a_running_task_whose_runner_vanished_is_swept_and_retried_on_the_next_candidate() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let id = f.work.submit(request(false)).await.unwrap();
    let held = store::Queue::at(&f.pool, now_ms())
        .claim(Way::Api, &[], "vanished")
        .await
        .unwrap()
        .expect("the queued task");
    assert_eq!((held.id, held.candidate.provider.as_str()), (id, "openai"));
    // Within its budget (half of the 60 s deadline) the runner is left alone.
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
    store::Queue::at(&f.pool, now_ms())
        .complete(
            store::CandidateAttempt::new(id, 0),
            &json!({"choice": "small"}),
            None,
        )
        .await
        .unwrap();
    let (answer, provider, _) = answered(wait(&f.work, id).await);
    assert_eq!(
        (answer["choice"].as_str(), provider.as_str()),
        (Some("large"), "anthropic")
    );
}

/// A service on `f`'s database whose openai and anthropic endpoints are
/// the given ones.
fn work_on(f: &Fixture, dir: &std::path::Path, openai: String, anthropic: String) -> InternalWork {
    let secret = secret_file(dir, 0o600);
    let config = InternalWorkConfig {
        api: [("openai", openai), ("anthropic", anthropic)]
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
    let registry = Some(registry(CredentialKind::ApiKey));
    let clock = aq_server::system_clock();
    InternalWork::new(f.pool.clone(), clock, Some(&config), registry).unwrap()
}

/// The loop stops a provider that never answers at the candidate budget
/// and reports it itself, well before the sweep would; the next
/// candidate gets what is left.
#[tokio::test]
async fn an_api_candidate_that_never_answers_is_reported_by_the_loop_not_the_sweep() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let dir = tempfile::tempdir().unwrap();
    let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
    let work = work_on(&f, dir.path(), fake::hang().await, anthropic);
    let _stop = start(&work);
    // Two candidates, 6 s: the floor gives the first all 6 s and the loop
    // stops it a tenth before that; the sweep would wait 5 s past it.
    let short = Request {
        deadline_ms: 6_000,
        ..request(true)
    };
    let id = work.submit(short).await.unwrap();
    let (claimed_at, deadline) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = store::get(&f.pool, id).await.unwrap().unwrap();
            if let (Some(at), "running") = (row.claimed_at_ms, row.status.as_str()) {
                return (at, row.deadline_at_ms);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the loop claims the task");
    let attempts = match wait(&work, id).await {
        Outcome::Answered {
            provider, attempts, ..
        } => {
            assert_eq!(provider, "anthropic");
            attempts
        }
        // Only on a machine too loaded to finish the second candidate in
        // the 0.6 s the first left it.
        Outcome::Failed { attempts, .. } => attempts,
    };
    let first = &attempts[0];
    assert_eq!(first.reason, Some(Reason::DeadlineExceeded));
    // The loop's detail ("within 6 s" unless the claim was slow): it
    // reported, not the sweep.
    assert!(
        first.detail.starts_with("openai did not answer within "),
        "{}",
        first.detail
    );
    // Not before the stop, the budget less a tenth of it here (spelled
    // out, so a flat 1 s margin fails), and before the sweep would have
    // taken it.
    let budget = candidate_budget_ms(deadline, claimed_at, 0, 2);
    let stop = (budget - budget / 10) as u64;
    let before_the_sweep = (budget + SWEEP_GRACE_MS) as u64;
    assert!(
        (stop..before_the_sweep).contains(&first.elapsed_ms),
        "{} ms, stop {stop}",
        first.elapsed_ms
    );
}

/// At stop the loop gives attempts in flight a few seconds to record
/// what they got, instead of dropping an answer on the floor.
#[tokio::test]
async fn at_stop_an_attempt_in_flight_still_records_its_answer() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let dir = tempfile::tempdir().unwrap();
    let delay = Duration::from_millis(800);
    let (openai, seen) = fake::slow(delay, fake::completed(SMALL)).await;
    let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
    let work = work_on(&f, dir.path(), openai, anthropic);
    let stop = start(&work);
    let id = work.submit(request(true)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen.lock().unwrap().bodies.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the provider call is in flight");
    drop(stop);
    let recorded = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = store::get(&f.pool, id).await.unwrap().unwrap();
            if row.status == "answered" {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the answer is recorded after the stop");
    assert_eq!(recorded.answer.unwrap()["choice"], "small");
}

/// The owner's requirement: internal work never waits behind other
/// internal work. A provider call that hangs for most of a 600 s
/// compile does not keep a blocking 60 s judgment from being claimed.
#[tokio::test]
async fn a_blocking_judgment_queued_behind_a_hanging_api_candidate_is_claimed_and_answered() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let dir = tempfile::tempdir().unwrap();
    let (openai, seen) = fake::hang_first(fake::completed(SMALL)).await;
    let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
    let work = work_on(&f, dir.path(), openai, anthropic);
    let _stop = start(&work);
    let compile = Request {
        deadline_ms: 600_000,
        ..request(false)
    };
    let compile = work.submit(compile).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen.lock().unwrap().bodies.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the compile's provider call is in flight");
    let judgment = work.submit(request(true)).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), work.wait(judgment))
        .await
        .expect("the judgment is answered while the compile hangs")
        .unwrap();
    let (answer, provider, attempts) = answered(outcome);
    assert_eq!(
        (answer["choice"].as_str(), provider.as_str()),
        (Some("small"), "openai")
    );
    assert_eq!(attempts.len(), 1);
    let row = store::get(&f.pool, compile).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.candidate_index), ("running", 0));
}

/// The sweep runs on its own interval: a dead login runner's task moves
/// on while an unrelated provider call is still in flight.
#[tokio::test]
async fn the_sweep_runs_while_a_provider_call_hangs() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let dir = tempfile::tempdir().unwrap();
    let (openai, seen) = fake::hang_first(fake::completed(SMALL)).await;
    let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
    let work = work_on(&f, dir.path(), openai, anthropic);
    let _stop = start(&work);
    let compile = Request {
        deadline_ms: 600_000,
        ..request(false)
    };
    work.submit(compile).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while seen.lock().unwrap().bodies.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the compile's provider call is in flight");
    // A login host claimed this one long ago and died.
    let logins = [login("anthropic", "claude"), login("openai", "codex")];
    let now = now_ms();
    let dead = store::submit(&f.pool, &request(false), &logins, now)
        .await
        .unwrap();
    let held = store::Queue::at(&f.pool, now - 200_000)
        .claim(Way::Login, &["claude"], "dead-host")
        .await
        .unwrap()
        .expect("the login task");
    assert_eq!(held.id, dead);
    let moved = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = store::get(&f.pool, dead).await.unwrap().unwrap();
            if row.candidate_index == 1 {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the sweep moved the dead runner's task on");
    assert_eq!(moved.status, "queued");
    assert_eq!(moved.attempts[0].detail, "runner did not report");
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
    let held = store::Queue::at(&f.pool, now)
        .claim(Way::Login, &["claude"], "dead-host")
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

/// The level `internal` saved again, listing only anthropic.
async fn configure_login_only(pool: &PgPool) {
    save_level(pool, &[("anthropic", "high", &["claude"])]).await;
}

#[tokio::test]
async fn internal_work_runs_while_swarm_availability_is_off_and_adds_no_swarm_admission_rows() {
    let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
    let owner = user(&f.pool).await;
    let mut conn = f.pool.acquire().await.unwrap();
    let off = aq_machine_control::set_owner_availability(
        &mut conn,
        false,
        aq_machine_control::OwnerChange {
            expected_generation: 1,
            actor: owner,
            operation_id: Uuid::new_v4(),
            now: chrono::Utc::now(),
        },
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
