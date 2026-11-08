//! Spec 0008 phase A: the live definition, usage, limits and holds (migration 0103).
use crate::common::sealed;
use uuid::Uuid;

const NOW: i64 = 1_800_000_000_000;

#[tokio::test]
async fn attempt_live_follows_the_hold_and_the_status() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let mut tx = sealed::replica(&pool).await;
    let held = sealed::attempt(&mut tx, true, None).await;
    let held_finished = sealed::attempt(&mut tx, true, Some("finished")).await;
    let released_granted = sealed::attempt(&mut tx, false, Some("granted")).await;
    let released_finished = sealed::attempt(&mut tx, false, Some("finished")).await;
    let released_never = sealed::attempt(&mut tx, false, Some("never_granted")).await;
    tx.commit().await.unwrap();
    let mut got = Vec::new();
    for id in [
        held,
        held_finished,
        released_granted,
        released_finished,
        released_never,
        Uuid::new_v4(),
    ] {
        let live: bool = sqlx::query_scalar("SELECT sealed_attempt_live($1)")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        got.push(live);
    }
    assert_eq!(got, vec![true, true, true, false, false, false]);
}

#[tokio::test]
async fn artifact_usage_counts_live_links_and_pending_rows_only() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = sealed::replica(&pool).await;
    let live = sealed::attempt(&mut tx, true, None).await;
    let settled = sealed::attempt(&mut tx, false, Some("finished")).await;
    let a1 = sealed::artifact(&mut tx, p, NOW - 5_000_000).await;
    sealed::link_artifact(&mut tx, settled, a1).await;
    sealed::link_artifact(&mut tx, live, a1).await;
    let a2 = sealed::artifact(&mut tx, p, NOW - 5_000_000).await;
    sealed::link_artifact(&mut tx, settled, a2).await;
    sealed::artifact(&mut tx, p, NOW - 1_000).await;
    sealed::artifact(&mut tx, p, NOW - 700_000).await;
    sealed::artifact(&mut tx, q, NOW - 1_000).await;
    tx.commit().await.unwrap();
    assert_eq!(
        sealed::usage(&pool, "admission_artifacts", Some(p), NOW, None).await,
        (3, 3, 2, 2)
    );
}

#[tokio::test]
async fn decision_usage_counts_linked_live_and_pending_only() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (node, p) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = sealed::replica(&pool).await;
    let live = sealed::attempt(&mut tx, true, None).await;
    let settled = sealed::attempt(&mut tx, false, Some("finished")).await;
    let d1 = sealed::decision(&mut tx, NOW - 5_000_000).await;
    sealed::offer(&mut tx, live, d1, node, p, NOW - 5_000_000).await;
    let d2 = sealed::decision(&mut tx, NOW - 5_000_000).await;
    sealed::offer(&mut tx, settled, d2, node, p, NOW - 5_000_000).await;
    sealed::decision(&mut tx, NOW - 1_000).await;
    sealed::decision(&mut tx, NOW - 700_000).await;
    tx.commit().await.unwrap();
    let u = sealed::usage(&pool, "intelligence_decisions", None, NOW, None).await;
    assert_eq!((u.0, u.2), (2, 2));
}

#[tokio::test]
async fn export_usage_counts_pending_and_live_linked_exports() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let p = Uuid::new_v4();
    let mut tx = sealed::replica(&pool).await;
    let live = sealed::attempt(&mut tx, true, None).await;
    let settled = sealed::attempt(&mut tx, false, Some("finished")).await;
    let e1 = sealed::export(&mut tx, p, "pending", 1_048_577).await;
    let e2 = sealed::export(&mut tx, p, "verified", 5000).await;
    sealed::link_export(&mut tx, live, e2).await;
    let e3 = sealed::export(&mut tx, p, "verified", 7000).await;
    sealed::link_export(&mut tx, settled, e3).await;
    sealed::export(&mut tx, p, "verified", 9000).await;
    tx.commit().await.unwrap();
    assert_eq!(
        sealed::usage(&pool, "source_exports", Some(p), 0, None).await,
        (2, 4_194_310, 2, 4_194_310)
    );
    assert_eq!(
        sealed::usage(&pool, "staging_exports", None, 0, None)
            .await
            .1,
        1_053_577
    );
    assert_eq!(
        sealed::usage(&pool, "source_exports", Some(p), 0, Some(e1))
            .await
            .2,
        1
    );
    let staging: i64 = sqlx::query_scalar("SELECT sealed_staging_live_bytes()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(staging, 1_053_577);
}

#[tokio::test]
async fn phase_terminal_and_registration_usage_count_live_attempts() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let n = Uuid::new_v4();
    let mut tx = sealed::replica(&pool).await;
    let live = sealed::attempt(&mut tx, true, None).await;
    let settled = sealed::attempt(&mut tx, false, Some("finished")).await;
    sealed::phase_capacity(&mut tx, live, n).await;
    sealed::phase_capacity(&mut tx, settled, n).await;
    sealed::registration(&mut tx, live, n).await;
    sealed::registration(&mut tx, settled, n).await;
    tx.commit().await.unwrap();
    assert_eq!(
        sealed::usage(&pool, "phase_terminal", Some(n), 0, None).await,
        (1, 1_048_576, 1, 1_048_576)
    );
    assert_eq!(
        sealed::usage(&pool, "launch_registrations", Some(n), 0, None).await,
        (1, 64, 1, 64)
    );
}

#[tokio::test]
async fn presence_usage_counts_unexpired_reports_and_the_hour() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, b) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = sealed::replica(&pool).await;
    sealed::inventory(&mut tx, n, b, 1, NOW - 1_000).await;
    sealed::inventory(&mut tx, n, b, 2, NOW - 60_000).await;
    sealed::inventory(&mut tx, n, b, 3, NOW - 60_000).await;
    sealed::inventory(&mut tx, n, b, 4, NOW - 3_700_000).await;
    tx.commit().await.unwrap();
    assert_eq!(
        sealed::usage(&pool, "presence_operations", Some(n), NOW, None).await,
        (1, 1028, 1, 1028)
    );
    assert_eq!(
        sealed::usage(&pool, "presence_rate", Some(n), NOW, None)
            .await
            .2,
        3
    );
}

#[tokio::test]
async fn offer_usage_counts_the_last_hour_per_node_and_project() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, m, p, q) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sealed::offers(&pool, n, p, NOW - 1_000, 3).await;
    sealed::offers(&pool, n, p, NOW - 3_700_000, 2).await;
    sealed::offers(&pool, m, q, NOW - 1_000, 1).await;
    assert_eq!(
        sealed::usage(&pool, "offers", Some(n), NOW, None).await,
        (4, 0, 3, 0)
    );
    assert_eq!(
        sealed::usage(&pool, "project_offers", Some(p), NOW, None).await,
        (4, 0, 3, 0)
    );
}

#[tokio::test]
async fn limits_are_the_spec_values() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let got: Vec<(String, String, i64, Option<i64>, i64, i64)> = sqlx::query_as(
        "SELECT kind,scope,max_rows,max_bytes,headroom_rows,headroom_bytes FROM sealed_history_limits() ORDER BY kind,scope",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let want: Vec<(&str, &str, i64, Option<i64>, i64, i64)> = vec![
        (
            "admission_artifacts",
            "global",
            4096,
            Some(268_435_456),
            64,
            4_194_304,
        ),
        (
            "admission_artifacts",
            "project",
            512,
            Some(67_108_864),
            64,
            4_194_304,
        ),
        (
            "intelligence_decisions",
            "global",
            1024,
            Some(67_108_864),
            2,
            16_777_216,
        ),
        ("launch_registrations", "global", 4096, Some(262_144), 1, 64),
        ("launch_registrations", "node", 256, Some(16_384), 1, 64),
        (
            "native_preparation",
            "global",
            4096,
            Some(67_108_864),
            1,
            262_144,
        ),
        (
            "native_preparation",
            "node",
            256,
            Some(4_194_304),
            1,
            262_144,
        ),
        ("offers", "global", 960, None, 1, 0),
        ("offers", "node", 30, None, 1, 0),
        (
            "phase_terminal",
            "global",
            256,
            Some(268_435_456),
            1,
            1_048_576,
        ),
        ("phase_terminal", "node", 16, Some(16_777_216), 1, 1_048_576),
        (
            "presence_operations",
            "global",
            65536,
            Some(268_435_456),
            1,
            0,
        ),
        ("presence_operations", "node", 4096, Some(16_777_216), 1, 0),
        ("presence_rate", "global", 11520, None, 1, 0),
        ("presence_rate", "node", 720, None, 1, 0),
        ("project_offers", "project", 240, None, 1, 0),
        (
            "source_exports",
            "global",
            4096,
            Some(268_435_456),
            1,
            4_202_496,
        ),
        (
            "source_exports",
            "project",
            256,
            Some(67_108_864),
            1,
            4_202_496,
        ),
        ("v2_proposals", "global", 4096, None, 1, 0),
        ("v2_proposals", "node", 256, None, 1, 0),
    ];
    let want: Vec<_> = want
        .into_iter()
        .map(|(k, s, r, b, hr, hb)| (k.to_string(), s.to_string(), r, b, hr, hb))
        .collect();
    assert_eq!(got, want);
}

type HoldRow = (String, String, String, String, i64, i64);

async fn holds(pool: &sqlx::PgPool, node: Option<Uuid>, project: Option<Uuid>) -> Vec<HoldRow> {
    sqlx::query_as(
        "SELECT hold_code,hold_kind,hold_scope,hold_unit,hold_used,hold_limit FROM sealed_history_holds($1,$2,$3)",
    )
    .bind(node)
    .bind(project)
    .bind(NOW)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn row(code: &str, kind: &str, scope: &str, unit: &str, used: i64, limit: i64) -> HoldRow {
    (
        code.into(),
        kind.into(),
        scope.into(),
        unit.into(),
        used,
        limit,
    )
}

#[tokio::test]
async fn holds_report_the_offer_rate_and_the_project_artifact_cap() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 30).await;
    let mut tx = sealed::replica(&pool).await;
    for _ in 0..449 {
        sealed::artifact(&mut tx, p, NOW - 1_000).await;
    }
    tx.commit().await.unwrap();
    assert_eq!(
        holds(&pool, Some(n), None).await,
        vec![row(
            "history_rate_limited",
            "offers",
            "node",
            "rows",
            30,
            30
        )]
    );
    assert_eq!(
        holds(&pool, None, Some(p)).await,
        vec![row(
            "history_cap_reached",
            "admission_artifacts",
            "project",
            "rows",
            449,
            512
        )]
    );
    sealed::offers(&pool, n, p, NOW - 1_000, 1).await;
    assert_eq!(
        holds(&pool, Some(n), None).await,
        vec![row(
            "history_rate_limited",
            "offers",
            "node",
            "rows",
            31,
            30
        )]
    );
}
