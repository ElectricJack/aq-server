//! Actual worker proof and daemon acknowledgments. Qualification is opt-in,
//! scoped to this pinned synthetic harness, never a real-provider advertisement.
use super::*;
use aq_phase_terminal_protocol as wire;
use aq_server::profiled_execution::{
    definitions::Definition, phase_terminal::qualification::TerminalQualification,
};
use sqlx::Row;

pub(super) async fn configure(case: &mut Case) {
    let mut config = (*case.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let r = &p.qualified_runtimes[0];
    let host = phase_host::configuration();
    let proof = serde_json::to_vec(&host).unwrap();
    let qualification = TerminalQualification {
        schema_version: 1,
        contract: wire::contract_artifact().unwrap(),
        runtime: r.runtime.clone(),
        runtime_qualification: r.qualification.clone(),
        proof_producer: ArtifactRef {
            id: Text::new("aq.synthetic-physical-proof/actual-process-test").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&proof),
        },
    };
    let body = serde_json::to_string(&qualification).unwrap();
    p.definition_bodies.push(
        Definition::new(
            ArtifactRef {
                id: Text::new("aq.phase-terminal-qualification/actual-process-test").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(body.as_bytes()),
            },
            aq_admission::ArtifactKind::Definition,
            body,
        )
        .unwrap(),
    );
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(Arc::new(p));
    case.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    if version == "32" {
        let mut tx = case.env.env.pool.begin().await.unwrap();
        sqlx::raw_sql(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/0033_phase_terminal.sql"
        )))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO schema_migrations(version) VALUES(33)")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    } else {
        let version = version.parse::<u32>().unwrap();
        if version != 33 {
            assert_eq!(version, aq_server::STATE_FORMAT_VERSION);
            assert!(version >= 33);
        }
    }
}
pub(super) async fn verify(case: &Case, root: &std::path::Path, result: &Value) {
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(root.join("journal/journal.json")).unwrap()).unwrap();
    let started: aq_admission::started::Request =
        serde_json::from_value(result["host_started"].clone()).unwrap();
    let terminal = &snapshot["state"]["attempts"][started.binding.attempt_id.to_string()]["native_host"]
        ["terminal"];
    let q: wire::StartedRequest =
        serde_json::from_value(terminal["started"]["request"].clone()).unwrap();
    let stop: wire::StoppedRequest =
        serde_json::from_value(terminal["stopped"]["request"].clone()).unwrap();
    let start_ack: wire::StartedReceipt =
        serde_json::from_value(result["phase_started_ack"].clone()).unwrap();
    let stop_ack: wire::StoppedReceipt =
        serde_json::from_value(result["phase_stopped_ack"].clone()).unwrap();
    start_ack.validate_for(&q).unwrap();
    stop_ack.validate_for(&stop).unwrap();
    for (table, expected, request_digest) in [
        (
            "profiled_started_receipts",
            &result["phase_started_ack"],
            q.digest().unwrap(),
        ),
        (
            "profiled_stopped_receipts",
            &result["phase_stopped_ack"],
            stop.digest().unwrap(),
        ),
    ] {
        let row = sqlx::query(&format!(
            "SELECT phase_ack_bytes,phase_request_digest FROM {table} WHERE attempt_id=$1"
        ))
        .bind(started.binding.attempt_id)
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap();
        let actual: Value =
            serde_json::from_slice(&row.get::<Vec<u8>, _>("phase_ack_bytes")).unwrap();
        assert_eq!(&actual, expected);
        assert_eq!(
            row.get::<String, _>("phase_request_digest"),
            request_digest.as_str()
        );
    }
    assert!(
        result["terminal_ack_revision"].as_u64().unwrap()
            > result["host_revision"].as_u64().unwrap()
    );
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(started.binding.attempt_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(hot["status"], "finished");
    assert_eq!(hot["stop"]["kind"], "stopped");
    let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM profiled_result_reports),(SELECT count(*) FROM profiled_verified_releases)")
        .fetch_one(&case.env.env.pool).await.unwrap();
    assert_eq!(counts, (0, 0));
    eprintln!(
        "actual worker Git/tmux lifecycle: both daemon ACKs persisted; server Finished/Stopped; settlement held"
    );
}
