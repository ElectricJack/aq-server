use super::*;

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
    host.document("routing").set(POLICY, 1, options());
    host.document("triage")
        .set("Label new tasks.", 1, json!({}));
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
    host.document("routing").set(POLICY, 2, options());
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
