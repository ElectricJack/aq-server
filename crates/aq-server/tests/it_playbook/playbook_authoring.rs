//! Playbook sources and authoring runs (spec 0013 §4.1, §4.5): the built-in
//! and registered source records, a run recorded per request, skipped when
//! nothing the agent would see changed, superseded by a newer request, and the
//! source and purpose found from an artifact digest. Resuming after a restart
//! submits and records each run's internal task exactly once.
use aq_internal_work::{Affected, AnswerShape, Purpose, Request, store as internal};
use aq_playbook_authoring::{
    self as authoring, Ended, Error, Finished, GRACE_MS, SourceKind, SourceRecord, active,
    ensure_builtin, finish, in_flight, register, requested, run_for_artifact, supersede_older,
    task_for,
};
use aq_server::test_support::TestDb;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

/// A source's Markdown, its revision and its facts.
type Now = (String, u64, BTreeMap<String, Value>);

/// What the host says each source's Markdown, revision and facts are now.
#[derive(Clone)]
struct Host {
    pool: PgPool,
    now: Arc<Mutex<BTreeMap<String, Now>>>,
    /// Read the pool too, as the server's host does (Settings, options).
    reads_pool: bool,
}

impl AsRef<PgPool> for Host {
    fn as_ref(&self) -> &PgPool {
        &self.pool
    }
}

impl authoring::Host for Host {
    async fn markdown(&self, source: &SourceRecord) -> authoring::Result<(String, u64)> {
        if self.reads_pool {
            sqlx::query("SELECT pg_sleep(0.02)")
                .execute(&self.pool)
                .await?;
        }
        let now = self.now.lock().unwrap();
        let (text, revision, _) = now.get(&source.id).cloned().unwrap_or_default();
        Ok((text, revision))
    }

    async fn facts(&self, source: &SourceRecord) -> authoring::Result<BTreeMap<String, Value>> {
        if self.reads_pool {
            sqlx::query("SELECT 1").execute(&self.pool).await?;
        }
        let now = self.now.lock().unwrap();
        Ok(now.get(&source.id).map(|s| s.2.clone()).unwrap_or_default())
    }
}

impl Host {
    fn set(&self, source: &str, text: &str, revision: u64, facts: Value) {
        let facts = facts.as_object().unwrap().clone().into_iter().collect();
        let mut now = self.now.lock().unwrap();
        now.insert(source.into(), (text.into(), revision, facts));
    }
}

async fn setup() -> (TestDb, Host) {
    let db = TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let host = Host {
        pool,
        now: Arc::default(),
        reads_pool: false,
    };
    (db, host)
}

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn routing() -> SourceRecord {
    SourceRecord {
        id: "routing".into(),
        kind: SourceKind::Builtin,
        binding: "aq.routing".into(),
        scope: "installation".into(),
        purpose: "Route every task to a level, a provider and a harness".into(),
        judgment_purpose: Purpose::RoutingClassification,
        commands: names(&["work.read", "work.route", "judgment.assess"]),
        events: names(&["aq.work.route_needed", "aq.judgment.assessed"]),
        capabilities: names(&["work.read", "work.route", "judgment.assess"]),
        facts: vec!["routing_options".into(), "structure".into()],
        revision: 0,
    }
}

fn triage(binding: &str) -> SourceRecord {
    SourceRecord {
        id: "triage".into(),
        kind: SourceKind::Registered,
        binding: binding.into(),
        scope: "installation".into(),
        purpose: "Label new tasks".into(),
        judgment_purpose: Purpose::PlaybookCall,
        commands: names(&["work.read", "judgment.assess"]),
        events: names(&["aq.judgment.assessed"]),
        capabilities: names(&["work.read", "judgment.assess"]),
        facts: vec![],
        revision: 0,
    }
}

const POLICY: &str = "Research is Standard, high effort, unless it is clearly harder.";

fn options() -> Value {
    json!({"routing_options": {"classes": {"standard-high": [["anthropic", "claude"]]}}})
}

fn save(digest: &str, activate: bool) -> Finished {
    Finished {
        status: Ended::Saved,
        activate,
        artifact_digest: Some(digest.into()),
        artifact: Some(json!({"digest": digest})),
        ..Finished::default()
    }
}

/// Records run `id` as saved with artifact `digest`, and active when `active`.
async fn saved(pool: &PgPool, id: i64, digest: &str, active: bool) {
    assert!(finish(pool, id, &save(digest, active)).await.unwrap());
}

async fn status(pool: &PgPool, id: i64) -> (String, Option<i64>) {
    sqlx::query_as("SELECT status,finished_at_ms FROM playbook_authoring_runs WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn runs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM playbook_authoring_runs")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_save_of_unchanged_text_and_facts_starts_no_run() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 3, options());
    let first = requested(&host, "routing")
        .await
        .unwrap()
        .expect("first run");
    assert_eq!(status(&pool, first).await, ("running".into(), None));
    // Nothing is active yet, so the same text asks again, and gets the run
    // already running for it.
    let again = requested(&host, "routing")
        .await
        .unwrap()
        .expect("no active run yet");
    assert_eq!(again, first);
    assert_eq!(runs(&pool).await, 1);
    saved(&pool, again, "sha256:first", true).await;

    let active_run = active(&pool, "routing").await.unwrap().expect("active");
    assert_eq!(active_run.id, again);
    assert_eq!(active_run.source_revision, 3);
    assert_eq!(active_run.source_digest, authoring::source_digest(POLICY));
    assert_eq!(active_run.artifact_digest, "sha256:first");
    let before = runs(&pool).await;

    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    // A later structure revision with the same text and facts (a description
    // edit elsewhere in the document) starts nothing either.
    host.set("routing", POLICY, 4, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(runs(&pool).await, before);

    // Changed facts start a run; the active one stays.
    host.set(
        "routing",
        POLICY,
        5,
        json!({"routing_options": {"classes": {}}}),
    );
    let changed = requested(&host, "routing")
        .await
        .unwrap()
        .expect("facts changed");
    assert_eq!(status(&pool, changed).await.0, "running");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, again);

    // A source nobody recorded is not found.
    let missing = requested(&host, "nothing").await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
}

#[tokio::test]
async fn a_newer_request_supersedes_a_running_one() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;

    host.set("routing", "Research is Standard.", 2, options());
    let second = requested(&host, "routing").await.unwrap().unwrap();
    host.set("routing", "Research is Complex.", 3, options());
    let third = requested(&host, "routing").await.unwrap().unwrap();
    assert!(first < second && second < third);

    let (second_status, finished) = status(&pool, second).await;
    assert_eq!(second_status, "superseded");
    assert!(finished.is_some());
    assert_eq!(status(&pool, third).await, ("running".into(), None));
    // A saved run is not running, so it is never superseded: it stays active.
    assert_eq!(status(&pool, first).await.0, "saved");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, first);

    // Only older runs of the same source are superseded.
    assert_eq!(supersede_older(&pool, "routing", third).await.unwrap(), 0);
    assert_eq!(supersede_older(&pool, "routing", second).await.unwrap(), 0);
    ensure_builtin(
        &pool,
        &SourceRecord {
            id: "other".into(),
            binding: "aq.other".into(),
            ..routing()
        },
    )
    .await
    .unwrap();
    host.set("other", POLICY, 1, options());
    let other = requested(&host, "other").await.unwrap().unwrap();
    assert_eq!(status(&pool, third).await.0, "running");
    assert_eq!(
        supersede_older(&pool, "routing", other + 1).await.unwrap(),
        1
    );
    assert_eq!(status(&pool, third).await.0, "superseded");
    assert_eq!(status(&pool, other).await.0, "running");
}

#[tokio::test]
async fn a_builtin_record_is_upserted_once() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    let stored = authoring::get(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        stored,
        SourceRecord {
            revision: 1,
            ..routing()
        }
    );
    let updated_at = |pool: PgPool| async move {
        let at: i64 =
            sqlx::query_scalar("SELECT updated_at_ms FROM playbook_sources WHERE id='routing'")
                .fetch_one(&pool)
                .await
                .unwrap();
        at
    };
    let at = updated_at(pool.clone()).await;

    ensure_builtin(
        &pool,
        &SourceRecord {
            revision: 9,
            ..routing()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        authoring::get(&pool, "routing").await.unwrap(),
        Some(stored)
    );
    assert_eq!(updated_at(pool.clone()).await, at);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_sources")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    // A built-in record has no stored Markdown: the host keeps it.
    assert_eq!(
        authoring::stored_markdown(&pool, "routing").await.unwrap(),
        None
    );

    // A newer server that widens the record updates it, once.
    let wider = SourceRecord {
        capabilities: names(&["work.read", "work.route", "judgment.assess", "work.note"]),
        ..routing()
    };
    ensure_builtin(&pool, &wider).await.unwrap();
    ensure_builtin(&pool, &wider).await.unwrap();
    let stored = authoring::get(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        stored,
        SourceRecord {
            revision: 2,
            ..wider
        }
    );

    // Only a built-in record is ensured this way.
    let wrong = ensure_builtin(&pool, &triage("aq.triage")).await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");
}

#[tokio::test]
async fn a_registered_source_rejects_a_binding_already_used() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();

    let taken = register(&pool, &triage("aq.routing"), "Label new tasks.").await;
    assert!(matches!(taken, Err(Error::Conflict(_))), "{taken:?}");
    assert_eq!(authoring::get(&pool, "triage").await.unwrap(), None);

    register(&pool, &triage("aq.triage"), "Label new tasks.")
        .await
        .unwrap();
    let stored = authoring::get(&pool, "triage").await.unwrap().unwrap();
    assert_eq!(
        stored,
        SourceRecord {
            revision: 1,
            ..triage("aq.triage")
        }
    );
    assert_eq!(
        authoring::stored_markdown(&pool, "triage").await.unwrap(),
        Some(("Label new tasks.".into(), 1))
    );

    let other = SourceRecord {
        id: "other".into(),
        ..triage("aq.triage")
    };
    let taken = register(&pool, &other, "Other.").await;
    assert!(matches!(taken, Err(Error::Conflict(_))), "{taken:?}");
    // The built-in source cannot be registered over.
    let builtin = SourceRecord {
        id: "routing".into(),
        ..triage("aq.elsewhere")
    };
    let taken = register(&pool, &builtin, "Route.").await;
    assert!(matches!(taken, Err(Error::Conflict(_))), "{taken:?}");
    assert_eq!(
        authoring::get(&pool, "routing")
            .await
            .unwrap()
            .unwrap()
            .kind,
        SourceKind::Builtin
    );
    // Registering needs a registered record.
    let wrong = register(&pool, &routing(), "Route.").await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");

    // Registering again with new Markdown updates it; the same Markdown does not.
    register(&pool, &triage("aq.triage"), "Label new tasks.")
        .await
        .unwrap();
    assert_eq!(
        authoring::get(&pool, "triage")
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    register(&pool, &triage("aq.triage"), "Label new tasks by area.")
        .await
        .unwrap();
    assert_eq!(
        authoring::stored_markdown(&pool, "triage").await.unwrap(),
        Some(("Label new tasks by area.".into(), 2))
    );
}

#[tokio::test]
async fn run_for_artifact_finds_the_source_and_purpose() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    register(&pool, &triage("aq.triage"), "Label new tasks.")
        .await
        .unwrap();
    host.set("routing", POLICY, 1, options());
    host.set("triage", "Label new tasks.", 1, json!({}));
    let route = requested(&host, "routing").await.unwrap().unwrap();
    let label = requested(&host, "triage").await.unwrap().unwrap();
    saved(&pool, route, "sha256:route", true).await;
    saved(&pool, label, "sha256:label", true).await;

    assert_eq!(
        run_for_artifact(&pool, "sha256:route").await.unwrap(),
        Some(("routing".into(), Purpose::RoutingClassification))
    );
    assert_eq!(
        run_for_artifact(&pool, "sha256:label").await.unwrap(),
        Some(("triage".into(), Purpose::PlaybookCall))
    );
    assert_eq!(
        run_for_artifact(&pool, "sha256:unknown").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_run_recorded_without_its_task_is_stranded_after_the_grace() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 2, options());
    let run = requested(&host, "routing").await.unwrap().unwrap();
    let created: i64 =
        sqlx::query_scalar("SELECT created_at_ms FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert!(
        authoring::stranded(&pool, created + GRACE_MS)
            .await
            .unwrap()
            .is_empty()
    );
    let late = authoring::stranded(&pool, created + GRACE_MS + 1)
        .await
        .unwrap();
    assert_eq!(late.len(), 1);
    assert_eq!(
        (
            late[0].id,
            late[0].source_id.as_str(),
            late[0].source_revision
        ),
        (run, "routing", 2)
    );
    assert_eq!(late[0].source_digest, authoring::source_digest(POLICY));

    let task = task_for(&pool, run).await.unwrap().expect("running");
    assert_eq!(task_for(&pool, run).await.unwrap(), Some(task));
    assert!(
        authoring::stranded(&pool, created + GRACE_MS + 1)
            .await
            .unwrap()
            .is_empty()
    );
    let recorded: Option<Uuid> =
        sqlx::query_scalar("SELECT internal_task_id FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recorded, Some(task));
}

#[tokio::test]
async fn a_changed_record_alone_starts_a_run() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    let active_run = active(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        active_run.record_digest,
        authoring::record_digest(&routing())
    );

    // A newer server widens the built-in record: same text, same facts.
    let wider = SourceRecord {
        commands: names(&["work.read", "work.route", "judgment.assess", "work.note"]),
        ..routing()
    };
    ensure_builtin(&pool, &wider).await.unwrap();
    let second = requested(&host, "routing")
        .await
        .unwrap()
        .expect("record changed");
    assert_eq!(status(&pool, second).await.0, "running");

    // A registered source given fewer commands is authored again too.
    register(&pool, &triage("aq.triage"), "Label new tasks.")
        .await
        .unwrap();
    host.set("triage", "Label new tasks.", 1, json!({}));
    let label = requested(&host, "triage").await.unwrap().unwrap();
    saved(&pool, label, "sha256:label", true).await;
    let narrower = SourceRecord {
        commands: names(&["work.read"]),
        ..triage("aq.triage")
    };
    register(&pool, &narrower, "Label new tasks.")
        .await
        .unwrap();
    // The record revision is the registered Markdown's revision.
    host.set("triage", "Label new tasks.", 2, json!({}));
    assert!(requested(&host, "triage").await.unwrap().is_some());
}

#[tokio::test]
async fn reverting_the_text_while_a_run_is_in_flight_supersedes_it() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;

    host.set("routing", "Research is Complex.", 2, options());
    let second = requested(&host, "routing").await.unwrap().unwrap();
    // The owner reverts to the active text before the second run finishes.
    host.set("routing", POLICY, 3, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(status(&pool, second).await.0, "superseded");

    // The second run's late result is not recorded and activates nothing.
    assert!(
        !finish(&pool, second, &save("sha256:second", true))
            .await
            .unwrap()
    );
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, first);
}

#[tokio::test]
async fn a_superseded_run_never_becomes_active() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;
    host.set("routing", "Research is Standard.", 2, options());
    let older = requested(&host, "routing").await.unwrap().unwrap();
    host.set("routing", "Research is Complex.", 3, options());
    let newer = requested(&host, "routing").await.unwrap().unwrap();

    // The newer result arrives first and takes effect in place of the first.
    assert!(
        finish(&pool, newer, &save("sha256:newer", true))
            .await
            .unwrap()
    );
    let now_active = active(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        (now_active.id, now_active.artifact_digest.as_str()),
        (newer, "sha256:newer")
    );
    let first_active: bool =
        sqlx::query_scalar("SELECT active FROM playbook_authoring_runs WHERE id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!first_active);
    assert_eq!(status(&pool, first).await.0, "saved");

    // The superseded run's result, in any order, records nothing.
    for ended in [save("sha256:older", true), Finished::default()] {
        assert!(!finish(&pool, older, &ended).await.unwrap());
    }
    assert_eq!(status(&pool, older).await.0, "superseded");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, newer);
    // And the newer run's result is recorded once.
    assert!(!finish(&pool, newer, &Finished::default()).await.unwrap());
    assert_eq!(status(&pool, newer).await.0, "saved");

    // Only a saved run with its artifact can be recorded as saved or active.
    host.set("routing", "Research is Trivial.", 4, options());
    let last = requested(&host, "routing").await.unwrap().unwrap();
    let not_saved = Finished {
        activate: true,
        ..Finished::default()
    };
    let wrong = finish(&pool, last, &not_saved).await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");
    let no_artifact = Finished {
        status: Ended::Saved,
        ..Finished::default()
    };
    let wrong = finish(&pool, last, &no_artifact).await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");
    assert_eq!(status(&pool, last).await.0, "running");
}

#[tokio::test]
async fn a_request_that_read_older_text_records_nothing() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", "Research is Complex.", 5, options());
    let newer = requested(&host, "routing").await.unwrap().unwrap();
    // A request that read revision 4 before revision 5 was saved arrives late.
    host.set("routing", POLICY, 4, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(status(&pool, newer).await.0, "running");
    assert_eq!(runs(&pool).await, 1);
}

#[tokio::test]
async fn concurrent_requests_with_a_host_that_reads_the_pool_all_finish() {
    let (_db, host) = setup().await;
    // The test pool has two connections; the host reads through it too.
    let host = Host {
        reads_pool: true,
        ..host
    };
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let requests = (0..6).map(|_| {
        let host = host.clone();
        tokio::spawn(async move { requested(&host, "routing").await })
    });
    let all = futures_util::future::join_all(requests);
    let all = tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .expect("no request waits on the pool");
    let ids: BTreeSet<i64> = (all.into_iter())
        .map(|r| r.unwrap().unwrap().expect("a run"))
        .collect();
    // One request recorded the run; the others found it running.
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(runs(&pool).await, 1);
}

fn authoring_request(run: i64) -> Request {
    Request {
        purpose: Purpose::RoutingClassification,
        instructions: "Author the routing playbook.".into(),
        input: "source.md".into(),
        answer: AnswerShape::Json { schema: json!({}) },
        blocking: false,
        affected: Affected {
            kind: "playbook_source".into(),
            id: run.to_string(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: false,
        profile_override: None,
        agent: None,
    }
}

async fn tasks(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM internal_tasks WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_run_stopped_between_its_task_id_and_submit_is_submitted_once() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let run = requested(&host, "routing").await.unwrap().unwrap();
    // The server gives the run its task id, then stops before submitting.
    let task = task_for(&pool, run).await.unwrap().unwrap();
    assert_eq!(tasks(&pool, task).await, 0);

    // After the restart the run is in flight with that id; two resumers race.
    let listed = in_flight(&pool).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|r| (r.id, r.internal_task_id))
            .collect::<Vec<_>>(),
        vec![(run, Some(task))]
    );
    let (one, two) = tokio::join!(task_for(&pool, run), task_for(&pool, run));
    assert_eq!((one.unwrap(), two.unwrap()), (Some(task), Some(task)));
    let request = authoring_request(run);
    let none: &[aq_internal_work::Candidate] = &[];
    let (one, two) = tokio::join!(
        internal::submit_as(&pool, task, &request, Ok(none), 1_000),
        internal::submit_as(&pool, task, &request, Ok(none), 2_000),
    );
    assert_eq!((one.unwrap(), two.unwrap()), (task, task));
    assert_eq!(tasks(&pool, task).await, 1);
    // A run no longer running gets no task id.
    assert!(finish(&pool, run, &Finished::default()).await.unwrap());
    assert_eq!(task_for(&pool, run).await.unwrap(), None);
}

#[tokio::test]
async fn a_task_finished_while_the_server_was_down_is_recorded_once() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.set("routing", POLICY, 1, options());
    let run = requested(&host, "routing").await.unwrap().unwrap();
    let task = task_for(&pool, run).await.unwrap().unwrap();
    // The task ends (here: no candidate) while nobody waits on it.
    internal::submit_as(
        &pool,
        task,
        &authoring_request(run),
        Err("no profile"),
        1_000,
    )
    .await
    .unwrap();
    let row = internal::get(&pool, task).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");

    // Two resumers find the run in flight and record its result: once.
    assert_eq!(in_flight(&pool).await.unwrap().len(), 1);
    let failed = Finished {
        refusal: Some("no profile".into()),
        ..Finished::default()
    };
    let (one, two) = tokio::join!(finish(&pool, run, &failed), finish(&pool, run, &failed));
    let recorded = [one.unwrap(), two.unwrap()];
    assert_eq!(recorded.iter().filter(|r| **r).count(), 1, "{recorded:?}");
    assert!(in_flight(&pool).await.unwrap().is_empty());
    let (ended, finished) = status(&pool, run).await;
    assert_eq!(ended, "failed");
    assert!(finished.is_some());
}
