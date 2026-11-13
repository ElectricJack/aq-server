//! Task 9 of the routing-policy plan: how tick time grows with the number of
//! routing runs in one protected namespace. Ignored; run by hand in a release
//! build (see docs/implementation/0011-R3-scale.md).
//!
//! This module is declared inside `real_adapter_pilots` so it can reuse the
//! private `Pilot` harness unchanged.

use super::{Error, Pilot, pilots};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use uuid::Uuid;

const CHECKPOINTS: [usize; 5] = [1, 50, 100, 250, 500];

async fn storage_report(p: &Pilot) -> String {
    let pool = p.f.db.pool().await;
    let mut out = vec![];
    for (table, filter) in [
        ("aq_playbooks.runtime_index", ""),
        ("aq_playbooks.inspection_rows", ""),
        ("aq_playbooks.catalog_rows", ""),
        ("aq_playbooks.namespaces", ""),
        ("aq_playbooks.disposal_tombstones", ""),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} {filter}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        out.push(format!("{table}={n}"));
    }
    let kinds: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind,count(*) FROM aq_playbooks.runtime_index GROUP BY kind ORDER BY kind",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    out.push(format!("runtime_index_kinds={kinds:?}"));
    let kinds: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind,count(*) FROM aq_playbooks.inspection_rows GROUP BY kind ORDER BY kind",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    out.push(format!("inspection_kinds={kinds:?}"));
    let size: Vec<(Option<i32>, Option<i32>, i64)> = sqlx::query_as(
        "SELECT pg_column_size(encrypted_snapshot),pg_column_size(snapshot),revision FROM aq_playbooks.namespaces",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    out.push(format!("snapshot(enc_bytes,plain_bytes,revision)={size:?}"));
    out.join(" ")
}

async fn measure() {
    // The routing work item is cloned from the node a coding run creates, so
    // its shape is the production one; that run happens in a throwaway pilot
    // so it adds no history to the measured namespace.
    let template = {
        let mut seed = Pilot::new().await;
        let policy = super::CodingPolicy::project_default(super::required_checks());
        let coding = policy.build().unwrap();
        let grant = seed.grant;
        let rev = seed.install(&coding.install_order(), "coding", grant).await;
        let run = seed
            .admit(
                "coding",
                rev,
                grant,
                policy.input(&super::coding_request(&seed.remote, "scale-seed")),
            )
            .await;
        let view = seed.settle(&run).await;
        assert_eq!(super::outcome(&view), ("completed", "published"), "{view}");
        let node = seed.graph.node(seed.graph.by_key("scale-seed/a0/work"));
        seed.driver.take().unwrap().stop().await.unwrap();
        node
    };

    let runs: usize = std::env::var("ROUTING_SCALE_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let mut p = Pilot::new().await;
    let grant = p.grant;
    let routing = pilots::routing().unwrap();
    let rev = p.install(&[&routing], "routing", grant).await;
    println!(
        "SCALE start runs={runs} storage: {}",
        storage_report(&p).await
    );
    let mut recent: Vec<Duration> = vec![];
    for n in 1..=runs {
        // A distinct work subject and a fresh attempt (no recorded facts, so
        // the run takes the judgment path, the longer of the two).
        let work_id = Uuid::new_v4();
        let mut node = template.clone();
        node["id"] = json!(work_id.to_string());
        node["metadata"]["aq:logical_key"] = json!(format!("scale-{n}"));
        // Not "active": the pilot lifecycle would patch (and so re-version) it.
        node["task_state"] = json!("completed");
        node["version"] = json!(1);
        p.graph.nodes.lock().unwrap().insert(work_id, node);
        // The row `work.ensure` leaves for a work item the namespace created;
        // `context.read` of a work subject refuses any other node.
        sqlx::query("INSERT INTO playbook_work_keys(namespace_id,generation,logical_key,task_id,request_digest,created_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(p.service.namespace.namespace_id)
            .bind(p.service.namespace.generation)
            .bind(format!("scale-{n}"))
            .bind(work_id)
            .bind("0".repeat(64))
            .bind(p.now())
            .execute(&p.f.db.pool().await)
            .await
            .unwrap();
        let input = json!({
            "attempt": Uuid::new_v4().to_string(),
            "work": {"id": work_id.to_string(), "revision": 1},
        });
        let started = Instant::now();
        let run = p.admit("routing", rev, grant, input).await;
        let mut slowest = Duration::ZERO;
        let mut ticks = 0usize;
        let mut view = Value::Null;
        for _ in 0..400 {
            let t = Instant::now();
            let result = p.driver().quantum().await;
            let took = t.elapsed();
            ticks += 1;
            slowest = slowest.max(took);
            recent.push(took);
            match result {
                Ok(_) | Err(Error::Overloaded) => {}
                Err(e) => {
                    let last: Vec<_> = recent.iter().rev().take(5).rev().collect();
                    println!(
                        "SCALE FAILED run={n} tick_error={e:?} after {took:?}; last five ticks={last:?}; storage: {}",
                        storage_report(&p).await
                    );
                    p.driver.take().unwrap().stop().await.ok();
                    return;
                }
            }
            view = p.view(&run).await;
            if matches!(
                view["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let total = started.elapsed();
        assert_eq!(
            super::outcome(&view),
            ("completed", "routed"),
            "run {n}: {view}"
        );
        println!(
            "SCALE run={n} slowest_tick_ms={} total_ms={} ticks={ticks} outcome={:?}",
            slowest.as_millis(),
            total.as_millis(),
            super::outcome(&view)
        );
        if CHECKPOINTS.contains(&n) || n == runs {
            println!(
                "SCALE checkpoint run={n} storage: {}",
                storage_report(&p).await
            );
        }
    }
    p.driver.take().unwrap().stop().await.unwrap();
}

/// 500 sequential routing runs in one protected namespace. The future is
/// large in a debug build, so it runs on a thread with a bigger stack.
#[test]
#[ignore = "measurement; run by hand in a release build"]
fn five_hundred_routing_runs_in_one_protected_namespace() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(measure()))
        })
        .unwrap()
        .join()
        .unwrap();
}
