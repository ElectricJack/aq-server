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
    type Limit = (String, String, i64, Option<i64>, i64, i64);
    let got: Vec<Limit> = sqlx::query_as(
        "SELECT kind,scope,max_rows,max_bytes,headroom_rows,headroom_bytes FROM sealed_history_limits() ORDER BY kind,scope",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let want = vec![
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

async fn holds(pool: &sqlx::PgPool, nodes: &[Uuid], projects: &[Uuid]) -> Vec<HoldRow> {
    let rows: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT hold_code,hold_kind,hold_scope,hold_unit,hold_used,hold_limit FROM sealed_history_holds($1,$2,$3)",
    )
    .bind(nodes)
    .bind(projects)
    .bind(NOW)
    .fetch_all(pool)
    .await
    .unwrap();
    rows
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
        holds(&pool, &[n], &[]).await,
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
        holds(&pool, &[], &[p]).await,
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
        holds(&pool, &[n], &[]).await,
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

#[tokio::test]
async fn offer_rate_fence_refuses_the_thirty_first_offer_in_an_hour() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let p = Uuid::new_v4();
    // 30 offers: the 31st insert is refused by the fence (54000). 29 offers: the
    // fence passes and the insert goes on to fail on the decision FK (23503).
    for (seeded, code) in [(30, "54000"), (29, "23503")] {
        let n = Uuid::new_v4();
        sealed::offers(&pool, n, p, NOW - 1_000, seeded).await;
        assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, code);
    }
}

#[tokio::test]
async fn project_and_server_offer_rates_hold_and_fence() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    // Project rate: 240 an hour, over distinct nodes so no node cap applies.
    let p = Uuid::new_v4();
    sealed::spread_offers(&pool, Some(p), 239, NOW - 1_000).await;
    assert_eq!(holds(&pool, &[], &[p]).await, vec![]);
    assert_eq!(
        sealed::try_offer(&pool, Uuid::new_v4(), p, NOW).await,
        "23503"
    );
    sealed::spread_offers(&pool, Some(p), 1, NOW - 1_000).await;
    assert_eq!(
        holds(&pool, &[], &[p]).await,
        vec![row(
            "history_rate_limited",
            "project_offers",
            "project",
            "rows",
            240,
            240
        )]
    );
    assert_eq!(
        sealed::try_offer(&pool, Uuid::new_v4(), p, NOW).await,
        "54000"
    );
    // Server rate: 960 an hour over distinct projects and nodes. 240 are in already.
    sealed::spread_offers(&pool, None, 719, NOW - 1_000).await;
    let n = Uuid::new_v4();
    assert!(holds(&pool, &[n], &[]).await.is_empty());
    assert_eq!(
        sealed::try_offer(&pool, n, Uuid::new_v4(), NOW).await,
        "23503"
    );
    sealed::spread_offers(&pool, None, 1, NOW - 1_000).await;
    assert_eq!(
        holds(&pool, &[n], &[]).await,
        vec![row(
            "history_rate_limited",
            "offers",
            "global",
            "rows",
            960,
            960
        )]
    );
    assert_eq!(
        sealed::try_offer(&pool, n, Uuid::new_v4(), NOW).await,
        "54000"
    );
}

#[tokio::test]
async fn inventory_report_rate_is_720_per_node_and_11520_in_all() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    // The 0029 lifetime counters are no longer charged: a node at their old
    // ceiling (4096) still reports.
    let n = sealed::node(&pool).await;
    sqlx::query("INSERT INTO profiled_presence_node_storage(node_id,operations) VALUES($1,4096) ON CONFLICT(node_id) DO UPDATE SET operations=4096")
        .bind(n).execute(&pool).await.unwrap();
    sealed::inventories(&pool, n, 718, NOW - 1_000).await;
    assert_eq!(holds(&pool, &[n], &[]).await, vec![]);
    // Under the limit: reports 719 and 720 are accepted.
    assert_eq!(sealed::report(&pool, n, 10_001, NOW).await, Ok(()));
    assert_eq!(sealed::report(&pool, n, 10_002, NOW).await, Ok(()));
    // At the limit: the 721st is refused, and the poll gate holds the node.
    assert_eq!(
        sealed::report(&pool, n, 10_003, NOW).await,
        Err("54000".to_string())
    );
    assert_eq!(
        holds(&pool, &[n], &[]).await,
        vec![row(
            "history_rate_limited",
            "presence_rate",
            "node",
            "rows",
            720,
            720
        )]
    );
    // Server-wide: 11,520 an hour over many nodes (718 + 2 above, already in).
    let m = sealed::node(&pool).await;
    for _ in 0..22 {
        sealed::inventories(&pool, Uuid::new_v4(), 500, NOW - 1_000).await;
    }
    // 720 + 11,000 = 11,720 > 11,520: the server cap refuses a node with room.
    assert_eq!(
        sealed::report(&pool, m, 1, NOW).await,
        Err("54000".to_string())
    );
    assert!(
        holds(&pool, &[m], &[])
            .await
            .iter()
            .any(|h| h.1 == "presence_rate" && h.2 == "global")
    );
}

#[tokio::test]
async fn live_caps_hold_at_the_limit_and_allow_below_it() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = sealed::replica(&pool).await;
    // 255 live registrations, 255 live exports, 255 live proposals: one below each cap of 256.
    for _ in 0..255 {
        let live = sealed::attempt(&mut tx, true, None).await;
        sealed::registration(&mut tx, live, n).await;
        sealed::proposal(&mut tx, live, n).await;
        let export = sealed::export(&mut tx, p, "verified", 5_000).await;
        sealed::link_export(&mut tx, live, export).await;
    }
    // 1,022 pending decisions: one below the cap of 1,024 less its headroom of 2.
    for _ in 0..1_022 {
        sealed::decision(&mut tx, NOW - 1_000).await;
    }
    tx.commit().await.unwrap();
    assert_eq!(holds(&pool, &[n], &[p]).await, vec![]);
    let mut tx = sealed::replica(&pool).await;
    let live = sealed::attempt(&mut tx, true, None).await;
    sealed::registration(&mut tx, live, n).await;
    sealed::proposal(&mut tx, live, n).await;
    let export = sealed::export(&mut tx, p, "verified", 5_000).await;
    sealed::link_export(&mut tx, live, export).await;
    sealed::decision(&mut tx, NOW - 1_000).await;
    tx.commit().await.unwrap();
    let got = holds(&pool, &[n], &[p]).await;
    assert_eq!(
        got,
        vec![
            row(
                "history_cap_reached",
                "intelligence_decisions",
                "global",
                "rows",
                1023,
                1024
            ),
            row(
                "history_cap_reached",
                "launch_registrations",
                "node",
                "rows",
                256,
                256
            ),
            row(
                "history_cap_reached",
                "source_exports",
                "project",
                "rows",
                256,
                256
            ),
            row(
                "history_cap_reached",
                "v2_proposals",
                "node",
                "rows",
                256,
                256
            ),
        ]
    );
}

#[tokio::test]
async fn unlinked_verified_exports_stay_live_for_the_pending_window() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let p = Uuid::new_v4();
    let mut tx = sealed::replica(&pool).await;
    sealed::export_created(&mut tx, p, "verified", 5_000, NOW - 1_000).await;
    sealed::export_created(&mut tx, p, "verified", 7_000, NOW - 700_000).await;
    tx.commit().await.unwrap();
    assert_eq!(
        sealed::usage(&pool, "staging_exports", None, NOW, None)
            .await
            .1,
        5_000
    );
    assert_eq!(
        sealed::usage(&pool, "source_exports", Some(p), NOW, None)
            .await
            .2,
        1
    );
    // Without a clock (0) the window is off, as for rows written before 0103.
    assert_eq!(
        sealed::usage(&pool, "staging_exports", None, 0, None)
            .await
            .1,
        0
    );
    let staging: i64 = sqlx::query_scalar("SELECT sealed_staging_live_bytes($1)")
        .bind(NOW)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(staging, 5_000);
}

#[tokio::test]
async fn holds_cover_only_the_listed_projects_and_nodes() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = sealed::replica(&pool).await;
    for project in [p, q] {
        for _ in 0..449 {
            sealed::artifact(&mut tx, project, NOW - 1_000).await;
        }
    }
    tx.commit().await.unwrap();
    assert_eq!(holds(&pool, &[], &[p]).await.len(), 1);
    assert_eq!(holds(&pool, &[], &[p, q]).await.len(), 2);
    assert!(holds(&pool, &[], &[Uuid::new_v4()]).await.is_empty());
}

#[tokio::test]
async fn settled_history_does_not_change_the_holds_or_the_usage() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 30).await;
    let mut tx = sealed::replica(&pool).await;
    for _ in 0..449 {
        sealed::artifact(&mut tx, p, NOW - 1_000).await;
    }
    let live = sealed::attempt(&mut tx, true, None).await;
    sealed::registration(&mut tx, live, n).await;
    tx.commit().await.unwrap();
    let kinds = [
        "admission_artifacts",
        "intelligence_decisions",
        "source_exports",
        "staging_exports",
        "native_preparation",
        "v2_proposals",
        "launch_registrations",
        "phase_terminal",
        "presence_operations",
        "presence_rate",
        "offers",
        "project_offers",
    ];
    let mut before = Vec::new();
    for k in kinds {
        before.push(sealed::usage(&pool, k, Some(n), NOW, None).await);
    }
    let held = holds(&pool, &[n], &[p]).await;
    assert!(!held.is_empty());
    sealed::settled_history(&pool, 2_000, 2_000, NOW, 86_400_000).await;
    let mut after = Vec::new();
    for k in kinds {
        after.push(sealed::usage(&pool, k, Some(n), NOW, None).await);
    }
    assert_eq!(before, after, "settled history is not counted");
    assert_eq!(held, holds(&pool, &[n], &[p]).await);
    let gate = aq_history_caps::holds(
        &mut pool.acquire().await.unwrap(),
        &[n],
        &[p],
        NOW as u64,
        Some((1 << 40, 1 << 20)),
    )
    .await
    .unwrap();
    assert_eq!(gate.len(), held.len());
}

/// The statements of `sealed_history_scoped`, as the poll gate runs them, with
/// the parameters replaced by literals.
fn scoped_statements(src: &str, live: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for kind in [
        "admission_artifacts",
        "intelligence_decisions",
        "source_exports",
        "staging_exports",
        "native_preparation",
        "v2_proposals",
        "launch_registrations",
        "phase_terminal",
        "presence_operations",
        "presence_rate",
        "offers",
        "project_offers",
    ] {
        let at = if kind.ends_with("_exports") {
            src.find("p_kind IN ('source_exports','staging_exports')")
        } else {
            src.find(&format!("p_kind='{kind}'"))
        }
        .unwrap();
        let rest = &src[at..];
        let start = rest.find("RETURN QUERY").unwrap() + "RETURN QUERY".len();
        let end = start + rest[start..].find(";\n ELS").unwrap();
        let sql = rest[start..end]
            .replace("p_now_ms", &NOW.to_string())
            .replace("p_exclude", "NULL::uuid")
            .replace("p_kind", &format!("'{kind}'"))
            .replace("ANY(live)", &format!("ANY('{live}'::uuid[])"));
        out.push((kind.to_string(), sql));
    }
    out
}

#[tokio::test]
async fn the_gate_and_the_presence_guard_do_not_scan_history() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 5).await;
    // A few live runs with history of their own, so the live set is not empty.
    let mut tx = sealed::replica(&pool).await;
    for _ in 0..40 {
        let live = sealed::attempt(&mut tx, true, None).await;
        sealed::registration(&mut tx, live, n).await;
        let artifact = sealed::artifact(&mut tx, p, NOW - 5_000_000).await;
        sealed::link_artifact(&mut tx, live, artifact).await;
    }
    tx.commit().await.unwrap();
    sealed::settled_history(&pool, 5_000, 50_000, NOW, 86_400_000).await;
    let live: String = sqlx::query_scalar(
        "SELECT COALESCE(array_agg(attempt_id),'{}')::text FROM sealed_live_attempt_ids",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(live.len() > 40, "the live set is not empty");
    let src: String =
        sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname='sealed_history_scoped'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let history = [
        "admission_artifacts",
        "intelligence_decisions",
        "profiled_source_exports",
        "profiled_launches",
        "profiled_launch_artifacts",
        "profiled_launch_source_exports",
        "admission_material_leases",
        "profiled_v2_proposals",
        "profiled_launch_registrations",
        "profiled_phase_terminal_capacity",
        "profiled_presence_operations",
        "admission_source_observations",
        "execution_attempts",
        "profiled_authority_hot",
    ];
    let mut report = String::new();
    for (kind, sql) in scoped_statements(&src, &live) {
        let plan: Vec<String> =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, BUFFERS, COSTS OFF) {sql}"))
                .fetch_all(&pool)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{kind}: {e}
{sql}"
                    )
                });
        let text = plan.join(
            "
",
        );
        report.push_str(&format!(
            "== {kind}
{text}
"
        ));
        for table in history {
            assert!(
                !text.contains(&format!("Seq Scan on {table}")),
                "{kind} scans {table}:
{text}"
            );
        }
    }
    if let Ok(path) = std::env::var("AQ_SEALED_PLANS") {
        std::fs::write(path, report).unwrap();
    }
}
#[tokio::test]
async fn phase_a_guards_call_the_live_usage() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    for (function, kind) in [
        ("admission_artifact_insert_guard", "admission_artifacts"),
        ("profiled_source_export_guard", "source_exports"),
        ("profiled_launch_registration_guard", "launch_registrations"),
        ("profiled_phase_terminal_capacity_guard", "phase_terminal"),
        ("profiled_presence_operation_guard", "presence_rate"),
    ] {
        let src: String = sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname=$1")
            .bind(function)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            src.contains(&format!("sealed_history_usage('{kind}'")),
            "{function}"
        );
    }
    let triggers: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname IN ('admission_artifact_insert','profiled_source_export_guard','profiled_launch_registration_guard','profiled_phase_terminal_capacity_guard','profiled_presence_operation_guard')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(triggers, 5);
}
