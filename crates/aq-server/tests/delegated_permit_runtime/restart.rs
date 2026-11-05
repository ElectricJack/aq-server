//! Fresh processes share only PostgreSQL, the configured key file and wire bytes.
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

async fn fresh_process(
    f: &phase_refresh_fixture::Prepared,
    now: u64,
    path: &str,
    permit: &DelegatedPermit,
    body: Value,
) -> (u16, Value) {
    use std::io::Write;
    use tokio::io::AsyncWriteExt;
    let mut key = tempfile::NamedTempFile::new().unwrap();
    key.write_all(&[7; 32]).unwrap();
    let output = tempfile::NamedTempFile::new().unwrap();
    let input = json!({
        "database":f.c.env.env._db.options().get_database().unwrap(),
        "now":now,"key":key.path(),"output":output.path(),
        "path":path,"permit":permit,"body":body,
        "profiled":f.c.env.env.state.config.profiled_execution.as_deref(),
    });
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart::server_restart_child",
            "--ignored",
            "--nocapture",
        ])
        .env("B2_RESTART_CHILD", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&serde_json::to_vec(&input).unwrap())
        .await
        .unwrap();
    drop(stdin);
    let status = child.wait_with_output().await.unwrap();
    assert!(
        status.status.success(),
        "restart child failed: {} {}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let result: Value = serde_json::from_reader(output.reopen().unwrap()).unwrap();
    (
        result["status"].as_u64().unwrap() as u16,
        result["body"].clone(),
    )
}

#[tokio::test]
#[ignore = "private subprocess entrypoint for server_restart_mid_attempt_resumes_or_preserves"]
async fn server_restart_child() {
    use std::{io::Read, str::FromStr};
    assert_eq!(std::env::var("B2_RESTART_CHILD").as_deref(), Ok("1"));
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).unwrap();
    let input: Value = serde_json::from_slice(&bytes).unwrap();
    let config_text = format!(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
enabled=false
[delegated_permits]
key_file={}
"#,
        input["key"]
    );
    let mut config: aq_server::config::Config = toml::from_str(&config_text).unwrap();
    config.profiled_execution = Some(Arc::new(
        serde_json::from_value(input["profiled"].clone()).unwrap(),
    ));
    let options =
        sqlx::postgres::PgConnectOptions::from_str(&std::env::var("DATABASE_URL").unwrap())
            .unwrap()
            .database(input["database"].as_str().unwrap());
    let now = input["now"].as_i64().unwrap();
    let state = aq_server::AppState {
        config: Arc::new(config),
        store: aq_server::store::Store::connect_with(options)
            .await
            .unwrap(),
        identity: Arc::new(aq_server::identity::DevIdentity::new(
            "owner@localhost".into(),
        )),
        clock: Arc::new(move || chrono::DateTime::from_timestamp_millis(now).unwrap()),
        agent_access: None,
        source_retention: None,
    };
    let permit: DelegatedPermit = serde_json::from_value(input["permit"].clone()).unwrap();
    let response = request(
        &state,
        input["path"].as_str().unwrap(),
        &headers(&permit),
        input["body"].clone(),
    )
    .await;
    let status = response.status().as_u16();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap(),
    )
    .unwrap();
    std::fs::write(
        input["output"].as_str().unwrap(),
        serde_json::to_vec(&json!({"status":status,"body":body})).unwrap(),
    )
    .unwrap();
}

pub(super) async fn exercise(
    f: &phase_refresh_fixture::Prepared,
    started: &aq_phase_terminal_protocol::refresh_v1::StartedRequest,
    q: &RunnerRenewalRequest,
) {
    let now =
        f.c.controlled_clock
            .as_ref()
            .unwrap()
            .load(Ordering::SeqCst) as u64;
    let body = serde_json::to_value(SandboxDocument::RunnerRenewalRequest(q.clone())).unwrap();
    let (status, response) =
        fresh_process(f, now, "/v1/worker/profiled/renew", &q.permit, body.clone()).await;
    assert_eq!(status, 200, "in-window renewal: {response}");
    let SandboxDocument::RunnerRenewalResponse(renewed) = serde_json::from_value(response).unwrap()
    else {
        panic!("renewal response")
    };
    let RenewalReply::Renewed { permit: live, .. } = &renewed.reply else {
        panic!("renewed permit")
    };
    assert!(live.expires_at_ms > q.permit.expires_at_ms);
    assert_eq!(live.hard_deadline_ms, q.permit.hard_deadline_ms);
    assert_eq!(renewed.revision, q.revision + 1);
    let (status, replay) =
        fresh_process(f, now, "/v1/worker/profiled/renew", &q.permit, body).await;
    assert_eq!(status, 200);
    let SandboxDocument::RunnerRenewalResponse(replay) = serde_json::from_value(replay).unwrap()
    else {
        panic!()
    };
    assert_eq!(replay.revision, renewed.revision);
    assert_eq!(
        serde_json::to_value(replay.reply).unwrap(),
        serde_json::to_value(&renewed.reply).unwrap()
    );

    let snapshot_sql = "SELECT jsonb_build_object('hot',(SELECT state FROM profiled_authority_hot LIMIT 1),'permit',(SELECT state FROM delegated_permit_current LIMIT 1),'ops',(SELECT count(*) FROM delegated_operations))";
    let before: Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    let expired = RunnerRenewalRequest {
        operation_id: Uuid::new_v4(),
        permit: live.clone(),
        revision: renewed.revision,
        ..q.clone()
    };
    let (status, refusal) = fresh_process(
        f,
        live.expires_at_ms,
        "/v1/worker/profiled/renew",
        live,
        serde_json::to_value(SandboxDocument::RunnerRenewalRequest(expired)).unwrap(),
    )
    .await;
    assert_eq!(status, 403);
    // The published native failure envelope redacts the internal reason.
    // HTTP 403/UnknownOrNotOwned is B6a2's refusal: stop and preserve.
    assert_eq!(refusal["kind"], "failure");
    assert_eq!(refusal["body"]["code"], "unknown_or_not_owned");
    let after: Value = sqlx::query_scalar(snapshot_sql)
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(before, after, "expired restart cannot extend authority");

    let stopped = phase_refresh_fixture::stop_request(started);
    let mut h = headers(live);
    h.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    assert_eq!(
        request(
            &f.c.env.env.state,
            "/v1/worker/profiled-v2/stopped",
            &h,
            serde_json::to_value(&stopped).unwrap()
        )
        .await
        .status(),
        200
    );
    let aq_admission::SessionIdentity::Remote {
        node_id, boot_id, ..
    } = started.original.binding.session
    else {
        panic!("remote attempt")
    };
    let report = aq_admission::transport::ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity { node_id, boot_id },
        binding: started.original.binding.clone(),
        supersedes: None,
        outcome: aq_admission::transport::ProcessOutcome::Success,
        stop: stopped.original.evidence.clone(),
        source_receipt_ids: vec![],
        pending_effect_ids: vec![],
        conclusion: aq_admission::Label::new("Restart result deduplication fixture").unwrap(),
    };
    let body = serde_json::to_value(&report).unwrap();
    let (status, receipt) =
        fresh_process(f, now, "/v1/worker/profiled/results", live, body.clone()).await;
    assert_eq!(status, 200, "result after restart: {receipt}");
    let (status, replay) = fresh_process(f, now, "/v1/worker/profiled/results", live, body).await;
    assert_eq!(status, 200);
    assert_eq!(receipt, replay);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1")
            .bind(live.binding.attempt_id)
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "restart must not duplicate the result");
}
