use super::*;

#[tokio::test]
async fn snapshots_and_receipts_roll_and_a_pinned_snapshot_stays_until_unpinned() {
    let f = started().await;
    let (_, shipped_digest) = f.active().await;
    let mut digests = check_rolling_limits(&f, &shipped_digest).await;
    // A live attempt that launched with digests[1] keeps it through later saves.
    let node = insert_node(&f).await;
    let attempt = insert_attempt(&f, node).await;
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

    check_active_only(&f).await;
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
    let node = insert_node(&f).await;
    let (_, first) = f.active().await;
    let live = launch(&f, node, &first).await;
    for n in 0..12 {
        let saved = f
            .save(Kind::Role, "developer", &developer(&format!("Rule {n}.")))
            .await
            .unwrap();
        // The save released the pin of the attempt that finished before it.
        assert_eq!(pin_count(&f).await, 1, "a finished attempt pins nothing");
        let attempt = launch(&f, node, &saved.digest).await;
        assert_eq!(pin_count(&f).await, 2);
        finish(&f, attempt).await;
        // The active one, the live attempt's, and at most `kept` others.
        let stored = f.stored_digests().await;
        assert!(stored.len() <= kept as usize + 2, "{stored:?}");
        assert!(stored.contains(&first), "the live attempt's snapshot stays");
    }
    // The live attempt ends: its snapshot is no longer kept by a pin, and the
    // next save leaves the active snapshot and `kept` others.
    finish(&f, live).await;
    f.save(Kind::Role, "developer", &developer("Last rule."))
        .await
        .unwrap();
    assert_eq!(pin_count(&f).await, 0);
    let stored = f.stored_digests().await;
    assert_eq!(stored.len(), kept as usize + 1, "{stored:?}");
    assert!(!stored.contains(&first));
    check_removed_attempt(&f, node).await;
}

async fn check_active_only(f: &Fixture) {
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

async fn check_removed_attempt(f: &Fixture, node: Uuid) {
    // An attempt whose record is gone pins nothing either, and the next
    // attempt's creation is enough to release it: no save is needed.
    let (_, active) = f.active().await;
    let removed = launch(f, node, &active).await;
    sqlx::query("DELETE FROM execution_attempts WHERE attempt_id=$1")
        .bind(removed)
        .execute(&f.pool)
        .await
        .unwrap();
    let next = insert_attempt(f, node).await;
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

async fn insert_node(f: &Fixture) -> Uuid {
    let node = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'pins','pins-hash','0.1.0',1,now())")
        .bind(node)
        .execute(&f.pool)
        .await
        .unwrap();
    node
}

async fn insert_attempt(f: &Fixture, node: Uuid) -> Uuid {
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,TRUE,'{}')")
        .bind(attempt).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&f.pool).await.unwrap();
    attempt
}

async fn launch(f: &Fixture, node: Uuid, digest: &str) -> Uuid {
    let attempt = insert_attempt(f, node).await;
    let mut conn = f.pool.acquire().await.unwrap();
    settings::pin(&mut conn, digest, attempt).await.unwrap();
    attempt
}
async fn finish(f: &Fixture, attempt: Uuid) {
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&f.pool)
        .await
        .unwrap();
}
async fn pin_count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM role_snapshot_pins")
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

async fn check_rolling_limits(f: &Fixture, shipped_digest: &str) -> Vec<String> {
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
        settings::snapshot(&f.pool, shipped_digest)
            .await
            .unwrap()
            .is_none()
    );
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM role_settings_operations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 2);

    digests
}
