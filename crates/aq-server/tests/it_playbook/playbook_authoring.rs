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
    /// What the fake engine was asked to do.
    engine: Arc<Mutex<Engine>>,
    /// A registered source's document is the stored one, as on the server.
    stored: bool,
}

/// A fake engine and grant store: it records installs, grants and activations.
#[derive(Default)]
struct Engine {
    installed: BTreeSet<String>,
    grants: BTreeMap<String, (Uuid, BTreeSet<String>)>,
    granted: usize,
    revoked: Vec<String>,
    /// `(binding, artifact digest)`, in order.
    activations: Vec<(String, String)>,
    down: bool,
    /// The engine refuses every activation (a passing fault).
    refuses_activation: bool,
    /// While an activation is under way, someone tries to delete this run;
    /// whether the delete had to wait is recorded.
    delete_during_activation: Option<i64>,
    delete_waited: Option<bool>,
}

impl authoring::Deployer for Host {
    async fn install(&self, artifact: &authoring::model::Artifact) -> authoring::Result<()> {
        let mut engine = self.engine.lock().unwrap();
        engine.installed.insert(artifact.digest.clone());
        Ok(())
    }
    async fn ensure_grant(
        &self,
        source: &str,
        capabilities: &BTreeSet<String>,
    ) -> authoring::Result<(Uuid, i64)> {
        let mut engine = self.engine.lock().unwrap();
        if engine.down {
            return Err(Error::Conflict("Playbooks are not running".into()));
        }
        if let Some((id, held)) = engine.grants.get(source)
            && held == capabilities
        {
            return Ok((*id, 1));
        }
        let id = Uuid::new_v4();
        engine.granted += 1;
        (engine.grants).insert(source.into(), (id, capabilities.clone()));
        Ok((id, 1))
    }
    async fn revoke_grant(&self, source: &str) -> authoring::Result<()> {
        let mut engine = self.engine.lock().unwrap();
        engine.grants.remove(source);
        engine.revoked.push(source.into());
        Ok(())
    }
    async fn activate(&self, _: &str, binding: &str, digest: &str) -> authoring::Result<()> {
        let doomed = self.engine.lock().unwrap().delete_during_activation.take();
        if let Some(run) = doomed {
            let delete =
                sqlx::query("DELETE FROM playbook_authoring_runs WHERE id=$1 AND NOT active")
                    .bind(run)
                    .execute(&self.pool);
            let waited = tokio::time::timeout(Duration::from_millis(400), delete).await;
            self.engine.lock().unwrap().delete_waited = Some(waited.is_err());
        }
        let mut engine = self.engine.lock().unwrap();
        if engine.refuses_activation {
            return Err(Error::Conflict("the engine is busy".into()));
        }
        assert!(
            engine.installed.contains(digest),
            "activated before install"
        );
        engine.activations.push((binding.into(), digest.into()));
        Ok(())
    }
}

fn login_candidate() -> aq_internal_work::Candidate {
    aq_internal_work::Candidate {
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        reasoning: None,
        reasoning_reference: "default".into(),
        options: BTreeMap::new(),
        harness: Some("claude".into()),
        way: aq_internal_work::Way::Login,
        credential_index: 0,
    }
}

fn clock_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.unwrap().as_millis() as i64
}

impl authoring::Server for Host {
    fn config(&self) -> authoring::Config {
        authoring::Config {
            keep_finished_runs: 3,
            facts_restarts: 1,
            ..authoring::Config::default()
        }
    }
    fn now_ms(&self) -> i64 {
        clock_ms()
    }
    async fn submit(&self, id: Uuid, request: &Request) -> authoring::Result<()> {
        (request.validate()).map_err(|e| Error::Invalid(e.to_string()))?;
        let candidates = [login_candidate()];
        internal::submit_as(&self.pool, id, request, Ok(&candidates), clock_ms())
            .await
            .map_err(authoring::complete::internal_error)?;
        Ok(())
    }
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
        if self.stored {
            let stored = authoring::stored_markdown(&self.pool, &source.id).await?;
            return stored.ok_or_else(|| Error::Host("no document".into()));
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
        engine: Arc::default(),
        stored: false,
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
        engine: Arc::default(),
        stored: false,
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

// ---- Workspace, save, take effect, rollback (plan tasks C3, C4) ----

/// A file of the engine's worked example `notify-when-ready`.
fn example(path: &str) -> &'static str {
    let path = format!("notify-when-ready/{path}");
    let files = authoring::compiler::docs::example_files();
    files.into_iter().find(|(p, _)| *p == path).unwrap().1
}

fn example_cases() -> Vec<Value> {
    (authoring::compiler::docs::example_files().into_iter())
        .filter(|(path, _)| path.starts_with("notify-when-ready/cases/"))
        .map(|(_, text)| serde_json::from_str(text).unwrap())
        .collect()
}

const NOTIFY: &str = "notify-when-ready";
const BINDING: &str = "aq.notify-when-ready";

fn notify() -> SourceRecord {
    SourceRecord {
        id: NOTIFY.into(),
        kind: SourceKind::Registered,
        binding: BINDING.into(),
        scope: "project".into(),
        purpose: "Ask for a description when a task has none".into(),
        judgment_purpose: Purpose::PlaybookCall,
        commands: names(&["message.send", "work.read"]),
        events: names(&["aq.work.changed"]),
        capabilities: names(&["message.send", "work.read"]),
        facts: vec![],
        revision: 0,
    }
}

fn notify_facts() -> Value {
    serde_json::from_str(example("facts.json")).unwrap()
}

/// Registers the worked example as a source whose document is `markdown`.
async fn notify_source(host: &Host, markdown: &str, revision: u64) {
    register(&host.pool, &notify(), markdown).await.unwrap();
    host.set(NOTIFY, markdown, revision, notify_facts());
}

/// The example's document with a closing line: the same playbook, reworded.
fn reworded(n: u64) -> String {
    format!("{}\nNote {n}.\n", example("source.md"))
}

/// What a fake agent leaves in `out/outcome.json` after `aq-playbooks save`.
fn saved_outcome() -> Value {
    json!({"kind": "saved", "draft": serde_json::from_str::<Value>(example("draft.json")).unwrap(),
        "cases": example_cases(), "artifact": format!("sha256:{}", "0".repeat(64)),
        "facts_digest": format!("sha256:{}", "0".repeat(64))})
}

fn answered(answer: Value) -> aq_internal_work::Outcome {
    aq_internal_work::Outcome::Answered {
        answer,
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        way: aq_internal_work::Way::Login,
        elapsed_ms: 1,
        usage: None,
        attempts: vec![],
    }
}

/// Authors the source and returns its run and internal task.
async fn authored(host: &Host) -> (i64, Uuid) {
    let run = authoring::author(host, NOTIFY)
        .await
        .unwrap()
        .expect("a run");
    let task: Uuid =
        sqlx::query_scalar("SELECT internal_task_id FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&host.pool)
            .await
            .unwrap();
    (run, task)
}

async fn source_view(host: &Host) -> Value {
    let record = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    authoring::view(&host.pool, &record, clock_ms())
        .await
        .unwrap()
}

/// The owner never reads an identifier in text meant for them.
fn owner_text(text: &str) {
    assert!(!text.contains("sha256:"), "{text}");
}

#[tokio::test]
async fn submitting_records_the_internal_task_on_the_run() {
    let (_db, host) = setup().await;
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    let row = internal::get(&host.pool, task).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.workspace_kind, "agent");
    assert_eq!(row.purpose, Purpose::PlaybookCompilation);
    assert_eq!(
        (row.affected_kind.as_str(), row.affected_id.as_str()),
        ("playbook_source", NOTIFY)
    );
    let agent = row.request.agent.unwrap();
    assert_eq!(agent.files["source.md"], example("source.md"));
    for path in [
        "GUIDE.md",
        "LANGUAGE.md",
        "CATALOG.md",
        "record.json",
        "catalog.json",
    ] {
        assert!(agent.files.contains_key(path), "{path}");
    }
    assert!(!agent.files.contains_key("previous.artifact.json"));
    // The grant exists before the run starts (ruling R2).
    assert_eq!(host.engine.lock().unwrap().granted, 1);
    // Asking again while it runs starts nothing new.
    assert_eq!(authored(&host).await, (run, task));
    assert_eq!(runs(&host.pool).await, 1);
    assert_eq!(source_view(&host).await["latest"]["status"], "running");
}

#[tokio::test]
async fn a_saved_playbook_is_installed_and_activated_and_the_previous_kept_for_rollback() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, first).await.0, "saved");
    let one = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!((one.id, one.previous_digest.clone()), (first, None));
    // The digest is the server's own seal, not the agent's claim.
    assert_ne!(one.artifact_digest, format!("sha256:{}", "0".repeat(64)));
    {
        let engine = host.engine.lock().unwrap();
        assert!(engine.installed.contains(&one.artifact_digest));
        assert_eq!(
            engine.activations,
            [(BINDING.to_string(), one.artifact_digest.clone())]
        );
    }
    let view = source_view(&host).await;
    assert_eq!(view["active"]["can_rollback"], false);
    assert_eq!(view["active"]["comparison"], Value::Null);
    let cases = view["active"]["cases"].as_array().unwrap();
    assert_eq!(cases.len(), example_cases().len());
    for case in cases {
        assert_eq!(case["passed"], true);
        owner_text(case["result"].as_str().unwrap());
    }
    // Unchanged text and facts: nothing to author.
    assert_eq!(authoring::author(&host, NOTIFY).await.unwrap(), None);

    // The owner rewords the document; the second agent is shown the first playbook.
    let reworded = format!("{}\nThank you.\n", example("source.md"));
    notify_source(&host, &reworded, 2).await;
    let (second, task) = authored(&host).await;
    let files = internal::get(&pool, task)
        .await
        .unwrap()
        .unwrap()
        .request
        .agent
        .unwrap()
        .files;
    assert!(files["previous.artifact.json"].contains(&one.artifact_digest));
    assert_eq!(files["previous/source.md"], example("source.md"));
    assert!(files.contains_key("previous/draft.json"));
    let previous_cases = files
        .keys()
        .filter(|p| p.starts_with("previous/cases/"))
        .count();
    assert_eq!(previous_cases, example_cases().len());
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let two = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(two.id, second);
    assert_ne!(two.artifact_digest, one.artifact_digest);
    assert_eq!(
        two.previous_digest.as_deref(),
        Some(one.artifact_digest.as_str())
    );
    let view = source_view(&host).await;
    assert_eq!(view["active"]["can_rollback"], true);
    assert_eq!(view["active"]["comparison"], json!({"change": "wording"}));
    assert!(view["active"]["previous"]["since_ms"].as_i64().unwrap() > 0);
    // Recording the same result twice changes nothing.
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(host.engine.lock().unwrap().activations.len(), 2);
}

#[tokio::test]
async fn rollback_reactivates_the_previous_artifact() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    // Nothing to go back to yet.
    assert!(matches!(
        authoring::rollback(&host, NOTIFY).await,
        Err(Error::Conflict(_))
    ));
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert!(matches!(
        authoring::rollback(&host, NOTIFY).await,
        Err(Error::Conflict(_))
    ));
    notify_source(&host, &reworded(2), 2).await;
    let (second, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // A third version is being authored when the owner rolls back.
    notify_source(&host, &reworded(3), 3).await;
    let (third, _) = authored(&host).await;

    authoring::rollback(&host, NOTIFY).await.unwrap();
    let now = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(now.id, first);
    let last = host
        .engine
        .lock()
        .unwrap()
        .activations
        .last()
        .cloned()
        .unwrap();
    assert_eq!(last, (BINDING.to_string(), now.artifact_digest.clone()));
    // The run in flight is superseded: the owner chose the previous playbook.
    assert_eq!(status(&pool, third).await.0, "superseded");
    assert_eq!(status(&pool, second).await.0, "saved");
    let view = source_view(&host).await;
    assert_eq!(view["active"]["artifact"], now.artifact_digest);
    assert_eq!(view["active"]["can_rollback"], false);
    assert_eq!(view["latest"]["status"], "superseded");
    // A later save of the document starts a new run.
    let (fourth, _) = authored(&host).await;
    assert!(fourth > third);
}

#[tokio::test]
async fn a_save_from_a_superseded_run_is_recorded_and_never_activated() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (older, older_task) = authored(&host).await;
    // The owner saves again before the first agent has finished.
    notify_source(&host, "Ask for a description.\n", 2).await;
    let (newer, _) = authored(&host).await;
    assert_ne!(older, newer);
    authoring::completed(&host, older_task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, older).await.0, "superseded");
    assert_eq!(status(&pool, newer).await.0, "running");
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    let engine = host.engine.lock().unwrap();
    assert!(engine.activations.is_empty() && engine.installed.is_empty());
}

#[tokio::test]
async fn a_save_whose_facts_changed_is_refused_and_authoring_starts_again() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    // Settings change while the agent works.
    host.set(
        NOTIFY,
        example("source.md"),
        1,
        json!({"owner": "user:owner-2"}),
    );
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let refusal: String =
        sqlx::query_scalar("SELECT refusal FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status(&pool, run).await.0, "refused");
    assert_eq!(refusal, authoring::FACTS_CHANGED);
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    assert!(host.engine.lock().unwrap().activations.is_empty());
    // Authoring started again for the new facts, with its task submitted.
    let again = in_flight(&pool).await.unwrap();
    assert_eq!(again.len(), 1);
    assert!(again[0].id > run);
    // It happens again: the restarts are capped (here at one), so it stops.
    host.set(
        NOTIFY,
        example("source.md"),
        1,
        json!({"owner": "user:owner-3"}),
    );
    let task = again[0].internal_task_id.unwrap();
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, again[0].id).await.0, "refused");
    assert!(in_flight(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_save_the_server_cannot_reproduce_is_refused_in_the_owners_words() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    let mut outcome = saved_outcome();
    outcome["cases"][0]["expect"]["outcome"] = json!("no-such-outcome");
    authoring::completed(&host, task, &answered(outcome))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "refused");
    let view = source_view(&host).await;
    let refusal = view["latest"]["refusal"].as_str().unwrap();
    assert!(refusal.contains("does not run as it says"), "{refusal}");
    owner_text(refusal);
    assert_eq!(view["active"], Value::Null);
    assert!(host.engine.lock().unwrap().installed.is_empty());
    // An outcome that is not one at all fails the run.
    let (run, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(json!({"kind": "saved"})))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");
}

#[tokio::test]
async fn a_question_or_unable_changes_nothing_and_keeps_the_sentence() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let lines: Vec<&str> = example("source.md").lines().collect();
    let (run, task) = authored(&host).await;
    // The agent's quote is a claim: the owner is shown the server's line.
    let question = json!({"kind": "question", "question": "How long is ten minutes?",
        "sentences": [{"line": 3, "end_line": 4, "text": "something the agent made up"}]});
    authoring::completed(&host, task, &answered(question))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "question");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["status"], "question");
    assert_eq!(
        view["latest"]["question"]["question"],
        "How long is ten minutes?"
    );
    let sentence = &view["latest"]["question"]["sentences"][0];
    assert_eq!(sentence["line"], 3);
    assert_eq!(sentence["text"], format!("{}\n{}", lines[2], lines[3]));
    assert!(view["latest"]["elapsed_ms"].as_i64().unwrap() >= 0);

    let (run, task) = authored(&host).await;
    let unable = json!({"kind": "unable", "reason": "Nothing can make a phone call.",
        "line": 1, "sentence": "not what line one says"});
    authoring::completed(&host, task, &answered(unable))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "unable");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["unable"]["sentence"], lines[0]);
    assert_eq!(view["latest"]["unable"]["line"], 1);
    assert_eq!(
        view["latest"]["unable"]["reason"],
        "Nothing can make a phone call."
    );

    // A fault in a host file is the server's, not the document's.
    let (run, task) = authored(&host).await;
    let unable = json!({"kind": "unable", "reason": "record.json allows a command the catalog lacks.",
        "file": "record.json"});
    authoring::completed(&host, task, &answered(unable))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "unable");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["unable"]["host_fault"], true);
    assert!(
        view["latest"]["unable"]["sentence"]
            .as_str()
            .unwrap()
            .contains("not in your document")
    );

    // A sentence that is not in the document fails the run.
    let (run, task) = authored(&host).await;
    let question = json!({"kind": "question", "question": "?",
        "sentences": [{"line": 4000, "text": "x"}]});
    authoring::completed(&host, task, &answered(question))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");

    // None of it changed what is in effect.
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    let engine = host.engine.lock().unwrap();
    assert!(engine.installed.is_empty() && engine.activations.is_empty());
}

#[tokio::test]
async fn a_failed_agent_fails_the_run_and_the_loop_records_what_ended() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    // Nothing has ended: a pass changes nothing.
    assert_eq!(authoring::tick(&host).await.unwrap(), 1);
    assert_eq!(status(&pool, run).await.0, "running");
    // A fake agent runs the task and leaves a prepared outcome.
    let claimed = internal::claim_login(&pool, &["claude"], true, "fake-agent", clock_ms())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, task);
    internal::complete(&pool, task, 0, &saved_outcome(), None, clock_ms())
        .await
        .unwrap();
    authoring::tick(&host).await.unwrap();
    assert_eq!(status(&pool, run).await.0, "saved");
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
    assert_eq!(authoring::tick(&host).await.unwrap(), 0);

    // An agent that ends without an outcome: the run fails, the playbook stays.
    notify_source(&host, "Ask for a description.\n", 2).await;
    let (run, task) = authored(&host).await;
    let failed = aq_internal_work::Outcome::Failed {
        reason: aq_internal_work::Reason::InvalidOutput,
        detail: "the agent ended without saving, asking or giving up".into(),
        attempts: vec![],
    };
    authoring::completed(&host, task, &failed).await.unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");
    let view = source_view(&host).await;
    assert!(
        view["latest"]["refusal"]
            .as_str()
            .unwrap()
            .contains("without saving")
    );
    assert!(view["active"]["artifact"].is_string());
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
}

#[tokio::test]
async fn finished_runs_are_a_rolling_set_that_keeps_the_active_and_the_previous() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    notify_source(&host, &reworded(2), 2).await;
    let (second, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // Six questions follow; the test host keeps three finished runs.
    for revision in 3..9 {
        notify_source(&host, &format!("Ask {revision} times.\n"), revision).await;
        let (_, task) = authored(&host).await;
        let question = json!({"kind": "question", "question": "How often?",
            "sentences": [{"line": 1, "text": "x"}]});
        authoring::completed(&host, task, &answered(question))
            .await
            .unwrap();
    }
    let kept: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,status FROM playbook_authoring_runs ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kept.len(), 5, "{kept:?}");
    // The active run and the one a rollback brings back are never deleted.
    assert_eq!((kept[0].0, kept[1].0), (first, second));
    assert!(kept[2..].iter().all(|(_, status)| status == "question"));
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, second);
    authoring::rollback(&host, NOTIFY).await.unwrap();
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, first);
}

#[tokio::test]
async fn after_a_restart_the_active_playbooks_are_installed_and_activated_again() {
    let (_db, host) = setup().await;
    notify_source(&host, example("source.md"), 1).await;
    let (_, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let digest = active(&host.pool, NOTIFY)
        .await
        .unwrap()
        .unwrap()
        .artifact_digest;
    // A new process with an engine that lost the artifact.
    *host.engine.lock().unwrap() = Engine::default();
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 1);
    let engine = host.engine.lock().unwrap();
    assert!(engine.installed.contains(&digest));
    assert_eq!(engine.activations, [(BINDING.to_string(), digest)]);
    assert_eq!(engine.granted, 1);
}

#[test]
fn the_authoring_catalog_is_the_host9_registry() {
    use aq_server::playbooks::review_domain::CatalogProfile;
    let pinned = authoring::compiler::docs::catalog_registry();
    assert!(pinned == CatalogProfile::Host9.registry());
}

// ---- The sources API (plan task C6) ----

/// The owner, as the server's extractor would give it.
struct Owner;
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Owner {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        _: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Owner)
    }
}
impl From<Owner> for Uuid {
    fn from(_: Owner) -> Uuid {
        Uuid::nil()
    }
}
struct Refused(Error);
impl From<Error> for Refused {
    fn from(e: Error) -> Self {
        Refused(e)
    }
}
impl axum::response::IntoResponse for Refused {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let status = match &self.0 {
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) | Error::Host(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, axum::Json(json!({"message": self.0.to_string()}))).into_response()
    }
}

struct Api(axum::Router);
impl Api {
    fn new(host: &Host) -> Self {
        let routes = authoring::http::routes::<Host, Owner, Refused>();
        Api(routes.with_state(host.clone()))
    }
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        let body = body.map_or_else(axum::body::Body::empty, |v| v.to_string().into());
        let response = self
            .0
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

const SOURCES: &str = "/api/playbooks/sources";

/// A host whose registered documents are the stored ones, with the facts
/// the worked example reads.
async fn api() -> (TestDb, Host, Api) {
    let (db, mut host) = setup().await;
    host.stored = true;
    host.set(NOTIFY, "", 0, notify_facts());
    let api = Api::new(&host);
    (db, host, api)
}

fn registration(markdown: &str) -> Value {
    json!({"id": NOTIFY, "binding": BINDING,
        "purpose": "Ask for a description when a task has none", "markdown": markdown,
        "commands": ["message.send", "work.read"], "events": ["aq.work.changed"]})
}

async fn latest_task(host: &Host) -> Uuid {
    in_flight(&host.pool).await.unwrap()[0]
        .internal_task_id
        .unwrap()
}

#[tokio::test]
async fn a_registered_source_is_authored_and_its_view_shows_the_cases() {
    let (_db, host, api) = api().await;
    assert_eq!(
        api.call("GET", SOURCES, None).await,
        (200, json!({"items": []}))
    );
    let (status, view) = api
        .call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        (&view["id"], &view["kind"], &view["binding"]),
        (&json!(NOTIFY), &json!("registered"), &json!(BINDING))
    );
    assert_eq!(view["active"], Value::Null);
    assert_eq!(view["latest"]["status"], "running");
    assert!(view["latest"]["run"].is_string());
    assert!(view["latest"]["internal_task_id"].is_string());
    assert!(view["latest"]["elapsed_ms"].is_number());
    // What the playbook may do follows from its commands, and the grant
    // exists before the run starts.
    let record = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(record.capabilities, names(&["message.send", "work.read"]));
    assert_eq!(
        host.engine.lock().unwrap().grants[NOTIFY].1,
        record.capabilities
    );

    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let (status, view) = api.call("GET", &format!("{SOURCES}/{NOTIFY}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(view["latest"]["status"], "saved");
    assert!(view["latest"]["finished_at_ms"].is_number());
    let active = &view["active"];
    assert!(active["artifact"].is_string() && active["since_ms"].as_i64().unwrap() > 0);
    assert_eq!(active["can_rollback"], false);
    assert_eq!(active["previous"], Value::Null);
    let cases = active["cases"].as_array().unwrap();
    assert_eq!(cases.len(), example_cases().len());
    for case in cases {
        for field in ["name", "situation", "result"] {
            assert!(case[field].is_string(), "{case}");
        }
        assert_eq!(case["passed"], true);
        owner_text(case["result"].as_str().unwrap());
    }
    let (_, list) = api.call("GET", SOURCES, None).await;
    assert_eq!(list["items"], json!([view]));

    // The owner changes the document: a new run, then a second playbook.
    let revision = view["revision"].as_u64().unwrap();
    let path = format!("{SOURCES}/{NOTIFY}/markdown");
    let body = json!({"markdown": reworded(2), "expected_revision": revision});
    let (status, view) = api.call("PUT", &path, Some(body.clone())).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["latest"]["status"], "running");
    assert_eq!(view["revision"], revision + 1);
    // A save over a revision someone else replaced is refused.
    assert_eq!(api.call("PUT", &path, Some(body)).await.0, 409);
    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // Author again with nothing changed starts nothing and answers the view.
    let (status, view) = api
        .call("POST", &format!("{SOURCES}/{NOTIFY}/runs"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(view["latest"]["status"], "saved");
    assert_eq!(view["active"]["can_rollback"], true);
    assert_eq!(view["active"]["comparison"]["change"], "wording");
    let second = view["active"]["artifact"].clone();
    // Rollback answers the refreshed view; a second one has nothing to go back to.
    let rollback = format!("{SOURCES}/{NOTIFY}/rollback");
    let (status, view) = api.call("POST", &rollback, None).await;
    assert_eq!(status, 200, "{view}");
    assert_ne!(view["active"]["artifact"], second);
    assert_eq!(view["active"]["can_rollback"], false);
    let (status, refused) = api.call("POST", &rollback, None).await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        api.call("POST", &format!("{SOURCES}/nobody/rollback"), None)
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn a_registration_names_commands_of_the_catalog_and_a_free_binding() {
    let (_db, host, api) = api().await;
    let mut unknown = registration("A document.\n");
    unknown["commands"] = json!(["work.delete"]);
    assert_eq!(api.call("POST", SOURCES, Some(unknown)).await.0, 400);
    let mut nameless = registration("A document.\n");
    nameless["id"] = json!("a/b");
    assert_eq!(api.call("POST", SOURCES, Some(nameless)).await.0, 400);
    let mut extra = registration("A document.\n");
    extra["capabilities"] = json!(["work.route"]);
    assert!(api.call("POST", SOURCES, Some(extra)).await.0 >= 400);
    assert_eq!(runs(&host.pool).await, 0);
    assert_eq!(
        api.call("POST", SOURCES, Some(registration("A document.\n")))
            .await
            .0,
        200
    );
    let mut taken = registration("Another.\n");
    taken["id"] = json!("other");
    assert_eq!(api.call("POST", SOURCES, Some(taken)).await.0, 409);
    // With no engine to deploy on, the source is refused in words.
    host.engine.lock().unwrap().down = true;
    let (status, refused) = api
        .call("POST", &format!("{SOURCES}/{NOTIFY}/runs"), None)
        .await;
    assert_eq!(status, 409);
    assert!(refused["message"].as_str().unwrap().contains("not running"));
}

#[tokio::test]
async fn put_markdown_on_a_builtin_source_is_refused() {
    let (_db, host, api) = api().await;
    // Before the server has written the built-in record there is no source.
    assert_eq!(
        api.call("GET", &format!("{SOURCES}/routing"), None).await.0,
        404
    );
    ensure_builtin(&host.pool, &routing()).await.unwrap();
    let (status, view) = api.call("GET", &format!("{SOURCES}/routing"), None).await;
    assert_eq!((status, &view["kind"]), (200, &json!("builtin")));
    assert_eq!(
        (&view["active"], &view["latest"]),
        (&Value::Null, &Value::Null)
    );
    let body = json!({"markdown": "Route everything to me.", "expected_revision": 1});
    let (status, refused) = api
        .call("PUT", &format!("{SOURCES}/routing/markdown"), Some(body))
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(runs(&host.pool).await, 0);
}

#[tokio::test]
async fn deleting_a_registered_source_revokes_its_grant_and_stops_its_runs() {
    let (_db, host, api) = api().await;
    let pool = host.pool.clone();
    api.call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    let task = latest_task(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let revision = source_view(&host).await["revision"].as_u64().unwrap();
    let body = json!({"markdown": reworded(2), "expected_revision": revision});
    api.call("PUT", &format!("{SOURCES}/{NOTIFY}/markdown"), Some(body))
        .await;
    let running = in_flight(&pool).await.unwrap()[0].id;

    let path = format!("{SOURCES}/{NOTIFY}");
    assert_eq!(
        api.call("DELETE", &path, None).await,
        (200, json!({"deleted": true}))
    );
    assert_eq!(host.engine.lock().unwrap().revoked, [NOTIFY]);
    assert_eq!(status(&pool, running).await.0, "superseded");
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    assert_eq!(api.call("GET", &path, None).await.0, 404);
    assert_eq!(api.call("GET", SOURCES, None).await.1, json!({"items": []}));
    assert_eq!(api.call("DELETE", &path, None).await.0, 404);
    // The run history stays, and a late result changes nothing.
    assert_eq!(runs(&pool).await, 2);
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 0);
    // Registering the id again brings the source back.
    let (status, view) = api
        .call("POST", SOURCES, Some(registration(example("source.md"))))
        .await;
    assert_eq!(
        (status, &view["latest"]["status"]),
        (200, &json!("running"))
    );
}

#[tokio::test]
async fn the_routing_source_cannot_be_deleted() {
    let (_db, host, api) = api().await;
    ensure_builtin(&host.pool, &routing()).await.unwrap();
    let (status, refused) = api
        .call("DELETE", &format!("{SOURCES}/routing"), None)
        .await;
    assert_eq!(status, 409, "{refused}");
    assert!(host.engine.lock().unwrap().revoked.is_empty());
    assert_eq!(
        api.call("GET", &format!("{SOURCES}/routing"), None).await.0,
        200
    );
}

// ---- Fix round (Codex review of C3 to C6) ----

/// Finding 3: a save whose engine activation fails stays the desired state,
/// and every pass tries again until the engine has it.
#[tokio::test]
async fn an_activation_that_fails_is_tried_again_until_the_engine_has_the_artifact() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    // Long after start: the start-up pass has nothing to put back.
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 0);
    let (run, task) = authored(&host).await;
    host.engine.lock().unwrap().refuses_activation = true;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "saved");
    let digest = active(&pool, NOTIFY)
        .await
        .unwrap()
        .unwrap()
        .artifact_digest;
    let deployed = || async {
        sqlx::query_scalar::<_, bool>("SELECT deployed FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert!(!deployed().await);
    assert!(host.engine.lock().unwrap().activations.is_empty());
    // Still failing: nothing is given up.
    authoring::tick(&host).await.unwrap();
    assert!(!deployed().await);
    // Authoring the unchanged text starts no run, so only the retry helps.
    assert_eq!(authoring::author(&host, NOTIFY).await.unwrap(), None);
    host.engine.lock().unwrap().refuses_activation = false;
    authoring::tick(&host).await.unwrap();
    assert!(deployed().await);
    let activations = host.engine.lock().unwrap().activations.clone();
    assert_eq!(activations, [(BINDING.to_string(), digest)]);
    // Acknowledged: later passes leave the engine alone.
    authoring::tick(&host).await.unwrap();
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
}

/// Finding 2: while a rollback is under way its target cannot be deleted, and
/// the rollback ends with exactly that run active.
#[tokio::test]
async fn a_rollback_holds_its_target_until_it_is_active() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    notify_source(&host, &reworded(2), 2).await;
    let (_, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    host.engine.lock().unwrap().delete_during_activation = Some(first);
    authoring::rollback(&host, NOTIFY).await.unwrap();
    assert_eq!(host.engine.lock().unwrap().delete_waited, Some(true));
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, first);
    // A rollback whose engine activation fails changes nothing.
    notify_source(&host, &reworded(3), 3).await;
    let (third, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    host.engine.lock().unwrap().refuses_activation = true;
    assert!(authoring::rollback(&host, NOTIFY).await.is_err());
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, third);
}

/// Findings 1 and 5: registration creates and never replaces, and a source
/// keeps its binding.
#[tokio::test]
async fn registering_an_existing_source_is_refused_and_keeps_its_document_and_binding() {
    let (_db, host, api) = api().await;
    let first = registration("The first document.\n");
    assert_eq!(api.call("POST", SOURCES, Some(first.clone())).await.0, 200);
    let revision = source_view(&host).await["revision"].as_u64().unwrap();
    let newer = json!({"markdown": "The owner's newer document.\n", "expected_revision": revision});
    let path = format!("{SOURCES}/{NOTIFY}/markdown");
    assert_eq!(api.call("PUT", &path, Some(newer)).await.0, 200);
    // A retry of the first registration does not bring the old text back.
    let (status, refused) = api.call("POST", SOURCES, Some(first)).await;
    assert_eq!(status, 409, "{refused}");
    let stored = authoring::stored_markdown(&host.pool, NOTIFY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.0, "The owner's newer document.\n");
    // No path moves a source to another binding.
    let moved = SourceRecord {
        binding: "aq.elsewhere".into(),
        ..notify()
    };
    let refused = register(&host.pool, &moved, "The owner's newer document.\n").await;
    assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    let kept = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(kept.binding, BINDING);
}
