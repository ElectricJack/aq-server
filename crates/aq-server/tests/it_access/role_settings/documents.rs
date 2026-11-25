use super::*;

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
        settings::Writer::new(&f.pool, f.owner, NOW).edit(revision, Uuid::new_v4()),
        Kind::Role,
        "reviewer",
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

/// Spec 0021 §5.1: Settings stored before the lineage time budgets were removed still load; the
/// keys are left out, and the next save writes the document without them.
#[tokio::test]
async fn stored_selection_defaults_with_the_removed_time_budgets_still_load() {
    let f = started().await;
    let mut stored = serde_json::to_value(SelectionDefaults::default()).unwrap();
    stored["max_attempts"] = json!(4);
    stored["max_lineage_elapsed_ms"] = json!(64_800_000);
    stored["max_lineage_sandbox_ms"] = json!(64_800_000);
    sqlx::query("UPDATE role_settings SET selection_defaults=$1 WHERE singleton")
        .bind(&stored)
        .execute(&f.pool)
        .await
        .unwrap();
    let read = settings::settings(&f.pool).await.unwrap();
    assert_eq!(read.selection_defaults.max_attempts, 4);
    let mut conn = f.pool.acquire().await.unwrap();
    let again = settings::selection_defaults(&mut conn).await.unwrap();
    assert_eq!(again, read.selection_defaults);
    drop(conn);
    let (status, body) = f.owner().get("/api/settings/selection-defaults").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let keys = body["selection_defaults"].as_object().unwrap();
    assert!(
        !keys.contains_key("max_lineage_elapsed_ms")
            && !keys.contains_key("max_lineage_sandbox_ms")
    );
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
        settings::delete(
            settings::Writer::new(&f.pool, f.owner, NOW).edit(revision, Uuid::new_v4()),
            kind,
            id,
        )
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
                settings::Writer::new(&pool, owner, NOW).edit(expected, operation),
                Kind::Role,
                "developer",
                &markdown,
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
        settings::Writer::new(&f.pool, viewer, NOW).edit(revision, operation),
        Kind::Role,
        "developer",
        &first,
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
                settings::Writer::new(&pool, owner, NOW).edit(expected, Uuid::new_v4()),
                &defaults,
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
    let refused = settings::save_limits(
        settings::Writer::new(&f.pool, f.owner, NOW).edit(0, Uuid::new_v4()),
        &tight,
    )
    .await;
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
