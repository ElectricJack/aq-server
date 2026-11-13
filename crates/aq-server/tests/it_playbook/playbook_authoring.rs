//! Playbook sources and authoring runs (spec 0013 §4.1, §4.5): the built-in
//! and registered source records, a run recorded per request, skipped when
//! nothing the agent would see changed, superseded by a newer request, and the
//! source and purpose found from an artifact digest.
use aq_internal_work::Purpose;
use aq_playbook_authoring::{
    self as authoring, Error, GRACE_MS, SourceKind, SourceRecord, active, ensure_builtin, register,
    requested, run_for_artifact, supersede_older,
};
use aq_server::test_support::TestDb;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// A source's Markdown, its revision and its facts.
type Now = (String, u64, BTreeMap<String, Value>);

/// What the host says each source's Markdown, revision and facts are now.
#[derive(Clone)]
struct Host {
    pool: PgPool,
    now: Arc<Mutex<BTreeMap<String, Now>>>,
}

impl AsRef<PgPool> for Host {
    fn as_ref(&self) -> &PgPool {
        &self.pool
    }
}

impl authoring::Host for Host {
    async fn markdown(&self, source: &SourceRecord) -> authoring::Result<(String, u64)> {
        let now = self.now.lock().unwrap();
        let (text, revision, _) = now.get(&source.id).cloned().unwrap_or_default();
        Ok((text, revision))
    }

    async fn facts(&self, source: &SourceRecord) -> authoring::Result<BTreeMap<String, Value>> {
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

/// Records run `id` as saved with artifact `digest`, and active when `active`.
async fn saved(pool: &PgPool, id: i64, digest: &str, active: bool) {
    sqlx::query(
        "UPDATE playbook_authoring_runs SET status='saved',active=$3,artifact_digest=$2,
            artifact='{}',finished_at_ms=created_at_ms+1 WHERE id=$1",
    )
    .bind(id)
    .bind(digest)
    .bind(active)
    .execute(pool)
    .await
    .unwrap();
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
    // Nothing is active yet, so the same text asks again.
    let again = requested(&host, "routing")
        .await
        .unwrap()
        .expect("no active run yet");
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

    let task = Uuid::new_v4();
    assert!(authoring::attach(&pool, run, task).await.unwrap());
    assert!(!authoring::attach(&pool, run, Uuid::new_v4()).await.unwrap());
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
