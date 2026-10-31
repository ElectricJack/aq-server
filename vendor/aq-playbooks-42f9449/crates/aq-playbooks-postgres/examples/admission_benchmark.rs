//! Small, bounded local storage workload; not a release performance threshold.
use aq_playbooks_model::*;
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{Authority, Engine};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")?;
    let namespace = format!(
        "benchmark-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let artifact = Artifact::seal(ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "benchmark-v1".into(),
        source: SourceBinding {
            digest: hash_bytes(b"bounded synthetic benchmark"),
            header: SourceHeader {
                format_version: 1,
                id: "admission-benchmark".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "admission-benchmark".into(),
            purpose: "Synthetic admission workload".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "return".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "return".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })?;
    let artifact_bytes = canonical_bytes(&artifact)?.len();
    let authority = Authority {
        actor: "synthetic-benchmark".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let engine = Engine::new(
        PostgresStorage::connect(&config)?,
        namespace,
        "benchmark-incarnation",
    )?;
    engine.install(artifact.clone())?;
    engine.activate("benchmark", &artifact.digest, authority.clone())?;
    let mut latency_us = Vec::with_capacity(100);
    let total = Instant::now();
    for n in 0..100 {
        let started = Instant::now();
        engine.admit("benchmark", "main", Value::Null, authority.clone(), n)?;
        latency_us.push(started.elapsed().as_micros() as u64);
    }
    let elapsed = total.elapsed();
    latency_us.sort_unstable();
    let snapshot_bytes = serde_json::to_vec(&engine.snapshot()?)?.len();
    engine.release()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"workload":"100 sequential root admissions; one return-only artifact; null inputs; growing retained namespace; no host effects","build":if cfg!(debug_assertions){"debug"}else{"release"},"admissions":100,"artifact_bytes":artifact_bytes,"final_state_json_bytes":snapshot_bytes,"p50_us":latency_us[49],"p95_us":latency_us[94],"maximum_us":latency_us[99],"elapsed_ms":elapsed.as_secs_f64()*1000.0,"admissions_per_second":100.0/elapsed.as_secs_f64()})
        )?
    );
    Ok(())
}
