//! aq-role-settings (spec 0014 §7, §7.1): the stored role and profile
//! documents, whole-set validation, the active snapshot, rolling snapshots
//! and the owner routes.
use aq_profiles::roles::{self, Kind, SelectionDefaults};
use aq_role_settings::{self as settings, Error, Receipt, StoreLimits};
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const NOW: i64 = 1_000_000;

struct Fixture {
    _db: TestDb,
    pool: PgPool,
    owner: Uuid,
    app: Router,
}
/// A migrated database with an owner, a viewer and the shipped defaults
/// installed and active, as after server start.
async fn started() -> Fixture {
    started_with("").await
}
/// [`started`], with `root` (top-level TOML keys) in the server's config.
async fn started_with(root: &str) -> Fixture {
    let db = TestDb::create().await;
    let store = db.store().await;
    let time = Utc.timestamp_millis_opt(NOW).unwrap();
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", time)
        .await
        .unwrap();
    store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, time)
        .await
        .unwrap();
    let config: Config = toml::from_str(&format!(
        r#"{root}
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
    ))
    .unwrap();
    let pool = db.pool().await;
    let owner = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(|| Utc.timestamp_millis_opt(NOW).unwrap()),
        agent_access: None,
        source_retention: None,
    };
    // `TestDb::store` has installed the shipped defaults, as server start does.
    let start = settings::startup(&pool, NOW).await.unwrap();
    assert!(start.installed.is_empty());
    assert!(start.problems.is_empty(), "{:?}", start.problems);
    Fixture {
        _db: db,
        pool,
        owner,
        app: aq_server::build_admin_router(state),
    }
}
fn shipped(kind: Kind, id: &str) -> String {
    let label = format!("/{id}.md");
    roles::default_documents()
        .into_iter()
        .find(|d| d.kind == kind && d.label.ends_with(&label))
        .unwrap()
        .text
}
/// The developer role with `text` appended to its rules: a valid edit.
fn developer(text: &str) -> String {
    format!("{}- {text}\n", shipped(Kind::Role, "developer"))
}
impl Fixture {
    async fn revision_of(&self, kind: Kind, id: &str) -> i64 {
        settings::get(&self.pool, kind, id)
            .await
            .unwrap()
            .map_or(0, |d| d.revision)
    }
    /// Save at the document's current revision with a fresh operation id.
    async fn save(&self, kind: Kind, id: &str, markdown: &str) -> Result<Receipt, Error> {
        let expected = self.revision_of(kind, id).await;
        settings::save(
            &self.pool,
            self.owner,
            kind,
            id,
            markdown,
            expected,
            Uuid::new_v4(),
            NOW,
        )
        .await
    }
    async fn limits(&self, snapshots_kept: u32, operations_kept: u32) {
        let current = settings::settings(&self.pool).await.unwrap();
        let limits = StoreLimits {
            snapshots_kept,
            operations_kept,
            ..current.limits
        };
        settings::save_limits(
            &self.pool,
            self.owner,
            &limits,
            current.limits_revision,
            Uuid::new_v4(),
            NOW,
        )
        .await
        .unwrap();
    }
    async fn active(&self) -> (i64, String) {
        let active = settings::active_snapshot(&self.pool)
            .await
            .unwrap()
            .unwrap();
        (active.revision, active.snapshot.digest().to_string())
    }
    async fn stored_digests(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT digest FROM role_snapshots ORDER BY activated_revision")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        owner: bool,
        csrf: bool,
    ) -> (StatusCode, Value) {
        let email = if owner {
            "owner@localhost"
        } else {
            "viewer@localhost"
        };
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("x-aq-dev-email", email);
        if csrf {
            request = request.header("X-AQ-Request", "1");
        }
        let body = Body::from(body.map(|v| v.to_string()).unwrap_or_default());
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}
fn problems(error: Error) -> Vec<String> {
    match error {
        Error::Problems(problems) => problems.iter().map(ToString::to_string).collect(),
        other => panic!("expected problems, got {other:?}"),
    }
}

#[tokio::test]
async fn defaults_are_installed_once_and_an_edit_is_not_overwritten() {
    let f = started().await;
    let shipped_set = roles::compile(
        &roles::default_documents(),
        &SelectionDefaults::default(),
        &Default::default(),
    )
    .unwrap();
    let (revision, digest) = f.active().await;
    assert_eq!(digest, shipped_set.digest());
    assert_eq!(settings::list(&f.pool, Kind::Role).await.unwrap().len(), 2);
    assert_eq!(
        settings::list(&f.pool, Kind::Intelligence)
            .await
            .unwrap()
            .len(),
        9
    );
    assert_eq!(
        settings::get(&f.pool, Kind::Execution, "sandbox")
            .await
            .unwrap()
            .unwrap()
            .markdown,
        shipped(Kind::Execution, "sandbox")
    );
    // A second start changes nothing.
    let again = settings::startup(&f.pool, NOW + 1).await.unwrap();
    assert!(again.installed.is_empty() && again.problems.is_empty());
    assert_eq!(f.active().await, (revision, digest.clone()));

    let edited = developer("Ask before deleting files.");
    let saved = f.save(Kind::Role, "developer", &edited).await.unwrap();
    assert_ne!(saved.digest, digest);
    let again = settings::startup(&f.pool, NOW + 2).await.unwrap();
    assert!(again.installed.is_empty());
    assert_eq!(again.active_digest.as_deref(), Some(saved.digest.as_str()));
    let stored = settings::get(&f.pool, Kind::Role, "developer")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.markdown, stored.revision), (edited, saved.revision));
    assert_eq!(f.active().await, (saved.revision, saved.digest));

    // A shipped default the owner deletes stays deleted at the next start.
    let revision = f.revision_of(Kind::Role, "reviewer").await;
    settings::delete(
        &f.pool,
        f.owner,
        Kind::Role,
        "reviewer",
        revision,
        Uuid::new_v4(),
        NOW,
    )
    .await
    .unwrap();
    let deleted = f.active().await;
    let again = settings::startup(&f.pool, NOW + 3).await.unwrap();
    assert!(again.installed.is_empty() && again.problems.is_empty());
    assert!(
        settings::get(&f.pool, Kind::Role, "reviewer")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.active().await, deleted);
    // A default this database has never installed (one a later release
    // adds) is installed at the next start, once.
    sqlx::query("DELETE FROM role_default_installs WHERE kind='role' AND id='reviewer'")
        .execute(&f.pool)
        .await
        .unwrap();
    let again = settings::startup(&f.pool, NOW + 4).await.unwrap();
    assert_eq!(again.installed, [(Kind::Role, "reviewer".to_string())]);
    let again = settings::startup(&f.pool, NOW + 5).await.unwrap();
    assert!(again.installed.is_empty());
}

#[tokio::test]
async fn a_stored_snapshot_is_read_as_stored_even_if_validation_would_refuse_it_today() {
    let f = started().await;
    let active = settings::active_snapshot(&f.pool).await.unwrap().unwrap();
    // A snapshot of an earlier day: a reasoning value no harness accepts now.
    let text = std::str::from_utf8(active.snapshot.canonical_bytes()).unwrap();
    let old = text.replace(r#""reasoning":"high""#, r#""reasoning":"ultra""#);
    assert_ne!(old, text);
    assert!(roles::Snapshot::from_canonical_bytes(old.as_bytes(), &Default::default()).is_err());
    let digest = roles::Snapshot::from_stored_bytes(old.as_bytes())
        .unwrap()
        .digest()
        .to_string();
    sqlx::query(
        "INSERT INTO role_snapshots(digest,snapshot,created_at_ms,activated_revision) VALUES($1,$2,$3,0)",
    )
    .bind(&digest)
    .bind(old.as_bytes())
    .bind(NOW)
    .execute(&f.pool)
    .await
    .unwrap();
    let read = settings::snapshot(&f.pool, &digest).await.unwrap().unwrap();
    assert_eq!(read.digest(), digest);
    assert_eq!(read.canonical_bytes(), old.as_bytes());
    assert_eq!(
        read.intelligence("standard-high").unwrap().candidates[0].reasoning,
        "ultra"
    );
    // The bytes must still be the ones the digest names.
    sqlx::query("UPDATE role_snapshots SET snapshot=$2 WHERE digest=$1")
        .bind(&digest)
        .bind(text.as_bytes())
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(matches!(
        settings::snapshot(&f.pool, &digest).await,
        Err(Error::Internal)
    ));
    // Compiling the current documents still validates fully.
    let bad = shipped(Kind::Intelligence, "standard-high").replace("| high |", "| ultra |");
    let refused = f.save(Kind::Intelligence, "standard-high", &bad).await;
    assert_eq!(
        problems(refused.unwrap_err()),
        [
            "intelligence profile `standard-high`: candidate 1: harness `claude` does not accept reasoning `ultra`"
        ]
    );
}

#[tokio::test]
async fn a_valid_save_activates_a_new_snapshot_and_an_invalid_one_changes_nothing() {
    let f = started().await;
    let (revision, digest) = f.active().await;
    let tester = shipped(Kind::Role, "reviewer")
        .replace("id: reviewer", "id: tester")
        .replace("name: Reviewer", "name: Tester");
    let saved = f.save(Kind::Role, "tester", &tester).await.unwrap();
    assert_eq!(saved.revision, revision + 1);
    assert_ne!(saved.digest, digest);
    assert_eq!(f.active().await, (saved.revision, saved.digest.clone()));
    let snapshot = settings::snapshot(&f.pool, &saved.digest)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.role("tester").is_some());
    snapshot.resolve("tester", "standard-high").unwrap();
    // The earlier snapshot is still stored, as one of the recent others.
    let earlier = settings::snapshot(&f.pool, &digest).await.unwrap().unwrap();
    assert!(earlier.role("tester").is_none());

    // Every problem of the resulting set is reported, and nothing changes.
    let broken = developer("x")
        .replace("execution: sandbox", "execution: nowhere")
        .replace("task.read", "task.fly");
    let found = problems(f.save(Kind::Role, "developer", &broken).await.unwrap_err());
    assert_eq!(
        found,
        [
            "role `developer`: execution: no execution profile `nowhere` exists",
            "role `developer`: operations: `task.fly` is not an AQ operation a role can hold",
        ]
    );
    let unreadable = problems(
        f.save(Kind::Role, "developer", "not a document")
            .await
            .unwrap_err(),
    );
    assert_eq!(
        unreadable,
        ["role document `developer`: line 1: a document starts with a `---` line"]
    );
    // A document is saved under the id it states.
    let misnamed = problems(f.save(Kind::Role, "qa", &tester).await.unwrap_err());
    assert_eq!(
        misnamed[0],
        "role document `qa`: id: is `tester`; a document saved as `qa` must have that id"
    );
    assert_eq!(f.active().await, (saved.revision, saved.digest));
    assert_eq!(
        settings::get(&f.pool, Kind::Role, "developer")
            .await
            .unwrap()
            .unwrap()
            .markdown,
        shipped(Kind::Role, "developer")
    );
    assert!(
        settings::get(&f.pool, Kind::Role, "qa")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn deleting_a_referenced_execution_profile_is_refused() {
    let f = started().await;
    let before = f.active().await;
    let revision = f.revision_of(Kind::Execution, "sandbox").await;
    let delete = |kind, id: &'static str, revision| {
        settings::delete(&f.pool, f.owner, kind, id, revision, Uuid::new_v4(), NOW)
    };
    let refused = problems(
        delete(Kind::Execution, "sandbox", revision)
            .await
            .unwrap_err(),
    );
    assert_eq!(
        refused,
        [
            "role `developer`: execution: no execution profile `sandbox` exists",
            "role `reviewer`: execution: no execution profile `sandbox` exists",
        ]
    );
    assert_eq!(f.active().await, before);
    assert!(
        settings::get(&f.pool, Kind::Execution, "sandbox")
            .await
            .unwrap()
            .is_some()
    );
    // Nothing refers to a level, so it can go; a missing document is not found.
    let revision = f.revision_of(Kind::Intelligence, "deep-high").await;
    let deleted = delete(Kind::Intelligence, "deep-high", revision)
        .await
        .unwrap();
    assert_eq!(f.active().await, (deleted.revision, deleted.digest));
    assert!(matches!(
        delete(Kind::Intelligence, "deep-high", revision).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn a_stale_revision_conflicts_and_an_operation_id_replays_once() {
    let f = started().await;
    let revision = f.revision_of(Kind::Role, "developer").await;
    let operation = Uuid::new_v4();
    let first = developer("First.");
    let save = |markdown: String, expected, operation| {
        let (pool, owner) = (f.pool.clone(), f.owner);
        async move {
            settings::save(
                &pool,
                owner,
                Kind::Role,
                "developer",
                &markdown,
                expected,
                operation,
                NOW,
            )
            .await
        }
    };
    let saved = save(first.clone(), revision, operation).await.unwrap();
    // The same request again: the stored receipt, and nothing moves.
    assert_eq!(
        save(first.clone(), revision, operation).await.unwrap(),
        saved
    );
    assert_eq!(f.active().await, (saved.revision, saved.digest.clone()));
    // The same operation id with another body or by another user is a conflict.
    assert!(matches!(
        save(developer("Second."), revision, operation).await,
        Err(Error::Conflict(_))
    ));
    let viewer: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role<>'owner'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let other = settings::save(
        &f.pool,
        viewer,
        Kind::Role,
        "developer",
        &first,
        revision,
        operation,
        NOW,
    )
    .await;
    assert!(matches!(other, Err(Error::Conflict(_))));
    // A new operation at the revision that was read before the save is stale.
    assert!(matches!(
        save(developer("Second."), revision, Uuid::new_v4()).await,
        Err(Error::Conflict(_))
    ));
    // Creating over an existing document is stale too; a nil id is refused.
    assert!(matches!(
        save(developer("Second."), 0, Uuid::new_v4()).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        save(developer("Second."), saved.revision, Uuid::nil()).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(f.active().await, (saved.revision, saved.digest));
}

#[tokio::test]
async fn snapshots_and_receipts_roll_and_a_pinned_snapshot_stays_until_unpinned() {
    let f = started().await;
    let (_, shipped_digest) = f.active().await;
    f.limits(1, 2).await;
    let mut digests = Vec::new();
    for n in 0..3 {
        let saved = f
            .save(Kind::Role, "developer", &developer(&format!("Rule {n}.")))
            .await
            .unwrap();
        digests.push(saved.digest);
    }
    // The active one and the one most recent other.
    assert_eq!(f.stored_digests().await, digests[1..]);
    assert!(
        settings::snapshot(&f.pool, &shipped_digest)
            .await
            .unwrap()
            .is_none()
    );
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM role_settings_operations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 2);

    // A live attempt that launched with digests[1] keeps it through later saves.
    let (attempt, node) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'pins','pins-hash','0.1.0',1,now())")
        .bind(node)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,TRUE,'{}')")
        .bind(attempt).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&f.pool).await.unwrap();
    let mut conn = f.pool.acquire().await.unwrap();
    settings::pin(&mut conn, &digests[1], attempt)
        .await
        .unwrap();
    settings::pin(&mut conn, &digests[1], attempt)
        .await
        .unwrap();
    assert!(matches!(
        settings::pin(&mut conn, &shipped_digest, Uuid::new_v4()).await,
        Err(Error::NotFound(_))
    ));
    for n in 3..6 {
        let saved = f
            .save(Kind::Role, "developer", &developer(&format!("Rule {n}.")))
            .await
            .unwrap();
        digests.push(saved.digest);
    }
    assert_eq!(
        f.stored_digests().await,
        [digests[1].clone(), digests[4].clone(), digests[5].clone()]
    );
    // Saving the pinned content again activates it without storing a copy.
    let again = f
        .save(Kind::Role, "developer", &developer("Rule 1."))
        .await
        .unwrap();
    assert_eq!(again.digest, digests[1]);
    assert_eq!(
        f.stored_digests().await,
        [digests[5].clone(), digests[1].clone()]
    );
    let last = f
        .save(Kind::Role, "developer", &developer("Rule 7."))
        .await
        .unwrap();
    let another = f
        .save(Kind::Role, "developer", &developer("Rule 8."))
        .await
        .unwrap();
    assert_eq!(
        f.stored_digests().await,
        [
            digests[1].clone(),
            last.digest.clone(),
            another.digest.clone()
        ]
    );
    // The attempt's record is removed: its snapshot goes with it.
    settings::unpin(&mut conn, attempt).await.unwrap();
    assert_eq!(
        f.stored_digests().await,
        [last.digest, another.digest.clone()]
    );

    // Keeping nothing is a valid setting, and no save is ever refused for
    // what is stored: only the active snapshot remains and no receipt.
    f.limits(0, 0).await;
    for n in 9..14 {
        f.save(Kind::Role, "developer", &developer(&format!("Rule {n}.")))
            .await
            .unwrap();
    }
    let (_, active) = f.active().await;
    assert_eq!(f.stored_digests().await, [active]);
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM role_settings_operations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
}

/// Spec 0014 §7.1: pins are for live attempts. Many saves, each with an
/// attempt that launched from it and then finished, leave the active
/// snapshot and at most `snapshots_kept` others; a live attempt's snapshot
/// survives the same saves, and goes when its attempt releases its task.
#[tokio::test]
async fn finished_attempts_release_their_snapshots_and_a_live_one_keeps_its_own() {
    let f = started().await;
    let kept = 2;
    f.limits(kept, 50).await;
    let node = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'pins','pins-hash','0.1.0',1,now())")
        .bind(node)
        .execute(&f.pool)
        .await
        .unwrap();
    // An attempt that holds its task, pinned to the active snapshot.
    let launch = |digest: String| {
        let pool = f.pool.clone();
        async move {
            let attempt = Uuid::new_v4();
            sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,TRUE,'{}')")
                .bind(attempt).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
                .execute(&pool).await.unwrap();
            let mut conn = pool.acquire().await.unwrap();
            settings::pin(&mut conn, &digest, attempt).await.unwrap();
            attempt
        }
    };
    let finish = |attempt: Uuid| {
        let pool = f.pool.clone();
        async move {
            sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
                .bind(attempt)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    let pins = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM role_snapshot_pins")
            .fetch_one(&f.pool)
            .await
            .unwrap()
    };
    let (_, first) = f.active().await;
    let live = launch(first.clone()).await;
    for n in 0..12 {
        let saved = f
            .save(Kind::Role, "developer", &developer(&format!("Rule {n}.")))
            .await
            .unwrap();
        // The save released the pin of the attempt that finished before it.
        assert_eq!(pins().await, 1, "a finished attempt pins nothing");
        let attempt = launch(saved.digest).await;
        assert_eq!(pins().await, 2);
        finish(attempt).await;
        // The active one, the live attempt's, and at most `kept` others.
        let stored = f.stored_digests().await;
        assert!(stored.len() <= kept as usize + 2, "{stored:?}");
        assert!(stored.contains(&first), "the live attempt's snapshot stays");
    }
    // The live attempt ends: its snapshot is no longer kept by a pin, and the
    // next save leaves the active snapshot and `kept` others.
    finish(live).await;
    f.save(Kind::Role, "developer", &developer("Last rule."))
        .await
        .unwrap();
    assert_eq!(pins().await, 0);
    let stored = f.stored_digests().await;
    assert_eq!(stored.len(), kept as usize + 1, "{stored:?}");
    assert!(!stored.contains(&first));
    // An attempt whose record is gone pins nothing either, and the next
    // attempt's creation is enough to release it: no save is needed.
    let (_, active) = f.active().await;
    let removed = launch(active.clone()).await;
    sqlx::query("DELETE FROM execution_attempts WHERE attempt_id=$1")
        .bind(removed)
        .execute(&f.pool)
        .await
        .unwrap();
    let next = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,TRUE,'{}')")
        .bind(next).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&f.pool).await.unwrap();
    let summary = aq_role_settings::attempts::RanWith {
        role: "developer".into(),
        level: "standard-high".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
        reasoning: "high".into(),
        harness: "claude".into(),
        execution_profile: "sandbox".into(),
        snapshot_digest: Some(active),
    };
    let mut conn = f.pool.acquire().await.unwrap();
    aq_role_settings::attempts::record(&mut conn, next, &summary)
        .await
        .unwrap();
    drop(conn);
    let pinned: Vec<Uuid> = sqlx::query_scalar("SELECT attempt_id FROM role_snapshot_pins")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(pinned, [next]);
}

#[tokio::test]
async fn selection_defaults_and_limits_are_settings_that_recompile_the_set() {
    let f = started().await;
    let (_, digest) = f.active().await;
    let read = settings::settings(&f.pool).await.unwrap();
    assert_eq!(read.selection_defaults, SelectionDefaults::default());
    assert_eq!(read.limits, StoreLimits::default());
    assert_eq!(
        (read.selection_defaults_revision, read.limits_revision),
        (0, 0)
    );
    let defaults = SelectionDefaults {
        max_attempts: 4,
        ..SelectionDefaults::default()
    };
    let save = |defaults: SelectionDefaults, expected| {
        let (pool, owner) = (f.pool.clone(), f.owner);
        async move {
            settings::save_selection_defaults(
                &pool,
                owner,
                &defaults,
                expected,
                Uuid::new_v4(),
                NOW,
            )
            .await
        }
    };
    let saved = save(defaults.clone(), 0).await.unwrap();
    assert_ne!(saved.digest, digest);
    let active = settings::active_snapshot(&f.pool).await.unwrap().unwrap();
    let launch = active.snapshot.resolve("developer", "simple-low").unwrap();
    assert_eq!(launch.selection_policy.max_attempts, 4);
    assert_eq!(launch.selection_policy.max_output_tokens, 32_000);
    assert!(matches!(
        save(defaults.clone(), 0).await,
        Err(Error::Conflict(_))
    ));
    let faulty = SelectionDefaults {
        max_attempts: 0,
        ..defaults.clone()
    };
    assert_eq!(
        problems(save(faulty, saved.revision).await.unwrap_err()),
        ["selection defaults: selection policy bounds"]
    );
    assert_eq!(
        settings::settings(&f.pool)
            .await
            .unwrap()
            .selection_defaults,
        defaults
    );

    // A limit the stored set does not fit is refused like any invalid change.
    let mut tight = StoreLimits::default();
    tight.documents.max_documents_per_kind = 8;
    let refused = settings::save_limits(&f.pool, f.owner, &tight, 0, Uuid::new_v4(), NOW).await;
    assert_eq!(
        problems(refused.unwrap_err()),
        ["all documents: hold 9 intelligence profile documents, over the limit of 8"]
    );
    assert_eq!(
        settings::settings(&f.pool).await.unwrap().limits,
        StoreLimits::default()
    );
}

#[tokio::test]
async fn an_invalid_stored_set_at_start_keeps_the_previous_snapshot() {
    let f = started().await;
    let before = f.active().await;
    sqlx::query("UPDATE role_documents SET markdown='damaged' WHERE kind='role' AND id='reviewer'")
        .execute(&f.pool)
        .await
        .unwrap();
    let start = settings::startup(&f.pool, NOW + 1).await.unwrap();
    assert_eq!(start.active_digest.as_deref(), Some(before.1.as_str()));
    assert_eq!(
        start
            .problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["role document `reviewer`: line 1: a document starts with a `---` line"]
    );
    assert_eq!(f.active().await, before);
    // Repairing the document through a save activates again.
    let repaired = f
        .save(Kind::Role, "reviewer", &shipped(Kind::Role, "reviewer"))
        .await
        .unwrap();
    assert_eq!(repaired.digest, before.1);
}

#[tokio::test]
async fn routes_require_the_owner_and_the_request_header() {
    let f = started().await;
    let put = json!({"markdown": developer("x"), "expected_revision": "1", "operation_id": Uuid::new_v4()});
    for path in [
        "/api/settings/roles",
        "/api/settings/roles/developer",
        "/api/settings/intelligence-profiles",
        "/api/settings/execution-profiles/sandbox",
        "/api/settings/selection-defaults",
        "/api/settings/profiles/limits",
        "/api/settings/profiles/snapshot",
    ] {
        let (status, body) = f.call("GET", path, None, false, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} {body}");
        let (status, body) = f.call("GET", path, None, true, false).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}");
    }
    let before = f.active().await;
    let one = "/api/settings/roles/developer";
    let (status, body) = f.call("PUT", one, Some(put.clone()), false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.call("PUT", one, Some(put), true, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let delete = format!(
        "/api/settings/intelligence-profiles/deep-high?expected_revision=1&operation_id={}",
        Uuid::new_v4()
    );
    let (status, _) = f.call("DELETE", &delete, None, false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f.call("DELETE", &delete, None, true, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.active().await, before);
}

#[tokio::test]
async fn an_owner_reads_saves_and_deletes_documents_over_http() {
    let f = started().await;
    let (revision, digest) = f.active().await;
    let (status, list) = f
        .call("GET", "/api/settings/roles", None, true, false)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["revision"], revision.to_string());
    assert_eq!(list["active_digest"], digest);
    let documents = list["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0]["id"], "developer");
    assert_eq!(documents[0]["markdown"], shipped(Kind::Role, "developer"));
    assert_eq!(documents[0]["fields"]["execution"], "sandbox");
    assert_eq!(
        documents[0]["fields"]["operations"],
        json!(["task.read", "knowledge.read", "source.write"])
    );
    assert_eq!(documents[0]["problem"], Value::Null);
    let (_, level) = f
        .call(
            "GET",
            "/api/settings/intelligence-profiles/standard-high",
            None,
            true,
            false,
        )
        .await;
    assert_eq!(
        level["fields"]["candidates"][0]["model"],
        "claude-sonnet-5-5"
    );
    let (_, sandbox) = f
        .call(
            "GET",
            "/api/settings/execution-profiles/sandbox",
            None,
            true,
            false,
        )
        .await;
    assert_eq!(sandbox["fields"]["confinement"], "container");
    let (status, _) = f
        .call("GET", "/api/settings/roles/tester", None, true, false)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Create a role.
    let tester = shipped(Kind::Role, "reviewer")
        .replace("id: reviewer", "id: tester")
        .replace("name: Reviewer", "name: Tester");
    let operation = Uuid::new_v4();
    let create = json!({"markdown": tester, "expected_revision": "0", "operation_id": operation});
    let path = "/api/settings/roles/tester";
    let (status, saved) = f.call("PUT", path, Some(create.clone()), true, true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], (revision + 1).to_string());
    let (_, replayed) = f.call("PUT", path, Some(create), true, true).await;
    assert_eq!(replayed, saved);
    let (_, snapshot) = f
        .call("GET", "/api/settings/profiles/snapshot", None, true, false)
        .await;
    assert_eq!(
        snapshot,
        json!({"revision": saved["revision"], "active_digest": saved["digest"]})
    );
    let (_, read) = f.call("GET", path, None, true, false).await;
    assert_eq!(read["revision"], saved["revision"]);
    assert_eq!(read["fields"]["name"], "Tester");

    // An invalid document: 400 with every problem; a stale revision: 409.
    let broken = tester
        .replace("execution: sandbox", "execution: nowhere")
        .replace("task.read", "task.fly");
    let body = json!({"markdown": broken, "expected_revision": saved["revision"], "operation_id": Uuid::new_v4()});
    let (status, refused) = f.call("PUT", path, Some(body), true, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"], "bad_request");
    assert_eq!(
        refused["problems"],
        json!([
            {"document": "role `tester`", "field": "execution", "message": "no execution profile `nowhere` exists"},
            {"document": "role `tester`", "field": "operations", "message": "`task.fly` is not an AQ operation a role can hold"},
        ])
    );
    let stale =
        json!({"markdown": tester, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", path, Some(stale), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = f
        .call("PUT", path, Some(json!({"markdown": tester})), true, true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Selection defaults.
    let (_, defaults) = f
        .call("GET", "/api/settings/selection-defaults", None, true, false)
        .await;
    assert_eq!(defaults["revision"], "0");
    assert_eq!(defaults["selection_defaults"]["max_attempts"], 2);
    let mut changed = defaults["selection_defaults"].clone();
    changed["max_attempts"] = json!(3);
    let body = json!({"selection_defaults": changed, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, moved) = f
        .call(
            "PUT",
            "/api/settings/selection-defaults",
            Some(body),
            true,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_ne!(moved["digest"], saved["digest"]);
    // A save keeps every key it does not state: a client that does not know the newer keys
    // (the dashboard form) cannot reset them. An explicit value still changes one, an
    // explicit `null` still clears a cost limit, and an unknown key is refused.
    let revision = || async {
        let read = f.call("GET", "/api/settings/selection-defaults", None, true, false);
        read.await.1["revision"].as_str().unwrap().to_owned()
    };
    let put = |body: Value, revision: String| {
        let body = json!({"selection_defaults": body, "expected_revision": revision, "operation_id": Uuid::new_v4()});
        let path = "/api/settings/selection-defaults";
        f.call("PUT", path, Some(body), true, true)
    };
    let stated = json!({"max_lineage_sandbox_ms": 7_200_000, "max_never_granted_offers": 3, "max_cost_microunits": 5, "currency": "USD"});
    let (status, body) = put(stated, revision().await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let partial = json!({"max_attempts": 4, "max_cost_microunits": null, "currency": null});
    let (status, body) = put(partial, revision().await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, read) = f
        .call("GET", "/api/settings/selection-defaults", None, true, false)
        .await;
    let now = &read["selection_defaults"];
    assert_eq!(now["max_attempts"], 4);
    assert_eq!(now["max_lineage_sandbox_ms"], 7_200_000, "kept: {now}");
    assert_eq!(now["max_never_granted_offers"], 3, "kept: {now}");
    assert_eq!(now["max_uncharged_continuations"], 5, "never stated: {now}");
    assert_eq!(now["max_cost_microunits"], Value::Null, "cleared: {now}");
    let (status, _) = put(json!({"no_such_key": 1}), revision().await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = put(json!({"max_never_granted_offers": 0}), revision().await).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the merged whole is validated"
    );
    let (_, limits) = f
        .call("GET", "/api/settings/profiles/limits", None, true, false)
        .await;
    assert_eq!(limits["limits"]["snapshots_kept"], 20);

    // Delete: refused while referenced, done otherwise.
    let delete = |path: &str, revision: &Value| {
        format!(
            "{path}?expected_revision={}&operation_id={}",
            revision.as_str().unwrap(),
            Uuid::new_v4()
        )
    };
    let (status, refused) = f
        .call(
            "DELETE",
            &delete(
                "/api/settings/execution-profiles/sandbox",
                &sandbox["revision"],
            ),
            None,
            true,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["problems"].as_array().unwrap().len(), 3);
    let (status, gone) = f
        .call(
            "DELETE",
            &delete(path, &saved["revision"]),
            None,
            true,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{gone}");
    let (status, _) = f.call("GET", path, None, true, false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = f
        .call("DELETE", "/api/settings/roles/tester", None, true, true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The old catalog's Settings and activation routes are gone.
    for path in [
        "/api/settings/intelligence",
        "/api/settings/intelligence/structure",
        "/api/settings/intelligence-extra",
        "/api/profiles/catalogs",
    ] {
        let (status, _) = f.call("GET", path, None, true, false).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn the_routing_document_is_saved_replayed_and_fenced_over_http() {
    const PATH: &str = "/api/settings/routing-document";
    let f = started().await;
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"markdown": "", "revision": "0"}));
    let (status, _) = f.call("GET", PATH, None, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read it");

    let put = json!({"markdown": "Route a one-line fix to simple-low.",
        "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), true, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let before = f.active().await;
    let (status, saved) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // Saving it compiles nothing: the same snapshot stays active.
    assert_eq!(saved["digest"], json!(before.1));
    assert_eq!(f.active().await.1, before.1);
    let (_, read) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(
        read,
        json!({"markdown": "Route a one-line fix to simple-low.", "revision": saved["revision"]})
    );

    // The same operation again answers as the first time and saves nothing.
    let (status, again) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // The operation id with another text is a conflict.
    let mut other = put.clone();
    other["markdown"] = json!("Something else.");
    let (status, body) = f.call("PUT", PATH, Some(other), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A save from a stale read is a conflict and changes nothing.
    let stale =
        json!({"markdown": "Late.", "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(stale), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.call("GET", PATH, None, true, false).await.1, read);
    // A next save names the revision it read.
    let next = json!({"markdown": "", "expected_revision": saved["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(next), true, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The store reads what the route saved (the authoring host reads this).
    let (markdown, revision) = settings::routing_document(&f.pool).await.unwrap();
    assert_eq!(
        (markdown.as_str(), revision.to_string()),
        ("", body["revision"].as_str().unwrap().to_owned())
    );
    // A document over the store's document limit is refused.
    let limit = StoreLimits::default().documents.max_document_bytes;
    let big = json!({"markdown": "x".repeat(limit + 1), "expected_revision": body["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(big), true, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn the_delivery_document_is_saved_replayed_and_fenced_over_http() {
    const PATH: &str = "/api/settings/delivery-document";
    let f = started().await;
    // Empty by default: no delivery policy.
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"markdown": "", "revision": "0"}));
    let (status, _) = f.call("GET", PATH, None, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read it");

    let put = json!({"markdown": "Merge when the checks pass.",
        "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), true, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "csrf_rejected");
    let before = f.active().await;
    let (status, saved) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // Saving it compiles nothing: the same snapshot stays active.
    assert_eq!(saved["digest"], json!(before.1));
    assert_eq!(f.active().await.1, before.1);
    let (_, read) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(
        read,
        json!({"markdown": "Merge when the checks pass.", "revision": saved["revision"]})
    );
    // The routing document is a separate place and is unchanged.
    let (_, routing) = f
        .call("GET", "/api/settings/routing-document", None, true, false)
        .await;
    assert_eq!(routing, json!({"markdown": "", "revision": "0"}));

    // The same operation again answers as the first time and saves nothing.
    let (status, again) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // The operation id with another text is a conflict.
    let mut other = put.clone();
    other["markdown"] = json!("Something else.");
    let (status, body) = f.call("PUT", PATH, Some(other), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A save from a stale read is a conflict and changes nothing.
    let stale =
        json!({"markdown": "Late.", "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(stale), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.call("GET", PATH, None, true, false).await.1, read);
    // A next save names the revision it read.
    let next = json!({"markdown": "", "expected_revision": saved["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(next), true, true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (markdown, revision) = settings::delivery_document(&f.pool).await.unwrap();
    assert_eq!(
        (markdown.as_str(), revision.to_string()),
        ("", body["revision"].as_str().unwrap().to_owned())
    );
    // A document over the store's document limit is refused.
    let limit = StoreLimits::default().documents.max_document_bytes;
    let big = json!({"markdown": "x".repeat(limit + 1), "expected_revision": body["revision"],
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(big), true, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // The body must be a markdown text and nothing else.
    let wrong = json!({"limits": {}, "expected_revision": "0", "operation_id": Uuid::new_v4()});
    let (status, _) = f.call("PUT", PATH, Some(wrong), true, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_delivery_limits_are_defaulted_validated_and_fenced_over_http() {
    use aq_role_settings::delivery::DeliveryLimits;
    const PATH: &str = "/api/settings/delivery-limits";
    let f = started().await;
    let defaults = json!({
        "main_sync_interval_ms": 60000, "main_freshness_ms": 30000,
        "decision_repeat_ms": 120000, "longest_check_wait_ms": 3600000,
        "git_transfer_timeout_ms": 600000, "max_push_bytes": 268435456,
        "require_landed_results": true});
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"limits": defaults, "revision": "0"}));
    let (status, _) = f.call("GET", PATH, None, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read them");

    let mut changed = defaults.clone();
    changed["main_sync_interval_ms"] = json!(15000);
    let put = json!({"limits": changed, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), true, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let before = f.active().await;
    let (status, saved) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(f.active().await.1, before.1, "nothing is compiled");
    let (_, read) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(
        read,
        json!({"limits": changed, "revision": saved["revision"]})
    );
    let (limits, revision) = settings::delivery_limits(&f.pool).await.unwrap();
    assert_eq!(limits.main_sync_interval_ms, 15000);
    assert_eq!(revision.to_string(), saved["revision"].as_str().unwrap());
    // The store's own limits keep their revision.
    let (_, own) = f
        .call("GET", "/api/settings/profiles/limits", None, true, false)
        .await;
    assert_eq!(own["revision"], "0");

    // The same operation again answers as the first time.
    let (status, again) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    // A stale read is a conflict and changes nothing.
    let stale = json!({"limits": defaults, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(stale), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.call("GET", PATH, None, true, false).await.1, read);

    // Invalid limits are refused and store nothing, whatever the fence says.
    let expected = saved["revision"].clone();
    let mut cases = Vec::new();
    for field in defaults.as_object().unwrap().keys() {
        let mut zero = defaults.clone();
        zero[field] = json!(0);
        cases.push(zero);
        let mut huge = defaults.clone();
        huge[field] = json!(u64::MAX);
        cases.push(huge);
        let mut negative = defaults.clone();
        negative[field] = json!(-1);
        cases.push(negative);
        // `require_landed_results` defaults to true when absent (rows stored
        // before it existed), so only the numbers are required.
        if field != "require_landed_results" {
            let mut missing = defaults.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push(missing);
        }
    }
    let mut unknown = defaults.clone();
    unknown["surprise"] = json!(1);
    cases.push(unknown);
    for limits in cases {
        let put = json!({"limits": limits, "expected_revision": expected,
            "operation_id": Uuid::new_v4()});
        let (status, body) = f.call("PUT", PATH, Some(put), true, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{limits}: {body}");
    }
    assert_eq!(f.call("GET", PATH, None, true, false).await.1, read);
    // Markdown is not a limits body.
    let wrong = json!({"markdown": "x", "expected_revision": expected,
        "operation_id": Uuid::new_v4()});
    let (status, _) = f.call("PUT", PATH, Some(wrong), true, true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(DeliveryLimits::default().validate(), Ok(()));
}

#[tokio::test]
async fn the_observation_limits_are_defaulted_validated_and_fenced_over_http() {
    const PATH: &str = "/api/settings/observation-limits";
    let f = started().await;
    // The defaults are the numbers that were constants before.
    let defaults = json!({
        "max_sources_per_node": 256, "max_events_per_node": 65536,
        "max_retained_bytes_per_node": 134217728, "max_calls_per_summary": 65536,
        "final_drain_grace_ms": 300000});
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"limits": defaults, "revision": "0"}));
    let (status, _) = f.call("GET", PATH, None, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer does not read them");

    let mut changed = defaults.clone();
    changed["max_sources_per_node"] = json!(1024);
    let put = json!({"limits": changed, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(put.clone()), false, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let before = f.active().await;
    let (status, saved) = f.call("PUT", PATH, Some(put.clone()), true, true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(f.active().await.1, before.1, "nothing is compiled");
    let (_, read) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(
        read,
        json!({"limits": changed, "revision": saved["revision"]})
    );
    // What a cap check reads on its own connection.
    let mut conn = f.pool.acquire().await.unwrap();
    let (limits, _) = settings::observation_limits(&mut conn).await.unwrap();
    assert_eq!(limits.max_sources_per_node, 1024);
    // The same operation again answers as the first time; a stale read is a
    // conflict; an invalid or unknown value is refused. Nothing changes.
    let (status, again) = f.call("PUT", PATH, Some(put), true, true).await;
    assert_eq!((status, &again), (StatusCode::OK, &saved));
    let stale = json!({"limits": defaults, "expected_revision": "0",
        "operation_id": Uuid::new_v4()});
    let (status, body) = f.call("PUT", PATH, Some(stale), true, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let mut cases = Vec::new();
    for field in defaults.as_object().unwrap().keys() {
        for bad in [json!(0), json!(u64::MAX), json!(-1)] {
            let mut limits = defaults.clone();
            limits[field] = bad;
            cases.push(limits);
        }
        let mut missing = defaults.clone();
        missing.as_object_mut().unwrap().remove(field);
        cases.push(missing);
    }
    let mut unknown = defaults.clone();
    unknown["surprise"] = json!(1);
    cases.push(unknown);
    for limits in cases {
        let put = json!({"limits": limits, "expected_revision": saved["revision"],
            "operation_id": Uuid::new_v4()});
        let (status, body) = f.call("PUT", PATH, Some(put), true, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{limits}: {body}");
    }
    assert_eq!(f.call("GET", PATH, None, true, false).await.1, read);
}

#[tokio::test]
async fn model_suggestions_are_read_from_the_discovery_cache_and_never_fail() {
    const PATH: &str = "/api/settings/model-suggestions";
    // No cache configured: nothing is suggested.
    let f = started().await;
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({"harnesses": []})));
    let (status, _) = f.call("GET", PATH, None, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("models.json");
    let root = format!(
        "intelligence_preferences_file={:?}",
        cache.to_str().unwrap()
    );
    let f = started_with(&root).await;
    // Configured but absent, and malformed: still an empty answer.
    for text in [None, Some("not json")] {
        if let Some(text) = text {
            std::fs::write(&cache, text).unwrap();
        }
        let (status, body) = f.call("GET", PATH, None, true, false).await;
        assert_eq!((status, &body), (StatusCode::OK, &json!({"harnesses": []})));
    }
    std::fs::write(
        &cache,
        json!([
            {"provider":"openai","model":"gpt-x","harness":"codex","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"},
            {"provider":"anthropic","model":"claude-y","harness":"claude"},
        ])
        .to_string(),
    )
    .unwrap();
    let (status, body) = f.call("GET", PATH, None, true, false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"harnesses": [
            {"harness":"claude","models":[{"provider":"anthropic","model":"claude-y","source":null,"observed_at":null}]},
            {"harness":"codex","models":[{"provider":"openai","model":"gpt-x","source":"Codex CLI","observed_at":"2026-11-16T00:00:00Z"}]},
        ]})
    );
    // A suggestion is not a gate: a level may name a model that is not listed.
    let level = "---\nid: standard-high\nname: Standard, high\ndescription: Ordinary work.\n---\n\
        ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n\
        | anthropic | a-model-nobody-suggested | high | claude |\n";
    f.save(Kind::Intelligence, "standard-high", level)
        .await
        .unwrap();
}
