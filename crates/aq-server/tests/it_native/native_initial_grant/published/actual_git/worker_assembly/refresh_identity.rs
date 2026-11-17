//! Independent identities for the pinned worker0.4.0 static fixture only.
//! No production/provider/tokenizer/privacy qualification is claimed.
use super::*;
use aq_admission::ArtifactKind;
use aq_server::profiled_execution::definitions::Definition;

fn publish(
    config: &mut aq_server::profiled_execution::config::ProfiledConfig,
    id: &str,
    body: Value,
) -> ArtifactRef {
    let body = serde_json::to_string(&body).unwrap();
    let artifact = ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    config
        .definition_bodies
        .push(Definition::new(artifact.clone(), ArtifactKind::Definition, body).unwrap());
    artifact
}
pub(super) async fn configure(case: &mut Case) {
    let executable = std::fs::canonicalize(
        std::env::var(WORKER_BINARY_ENV).expect("explicit refresh worker binary"),
    )
    .unwrap();
    let expected: Digest = serde_json::from_value(json!(
        std::env::var(WORKER_DIGEST_ENV).expect("explicit refresh worker digest")
    ))
    .unwrap();
    assert_eq!(Digest::of(&std::fs::read(&executable).unwrap()), expected);
    let source = std::env::var("AQ_TEST_REFRESH_WORKER_SOURCE")
        .expect("explicit frozen refresh worker source commit");
    assert!(source.len() == 40 && source.bytes().all(|b| b.is_ascii_hexdigit()));
    let host = phase_host::configuration();
    let mut config = (*case.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    assert_eq!(profiled.qualified_runtimes.len(), 1);
    let runtime = publish(
        &mut profiled,
        "aq.static-worker-runtime/0.4.0-refresh-output-v1",
        json!({
            "scope":"independent static peer only; no provider, native-model or source-privacy qualification",
            "worker_version":"0.4.0","worker_source":source,"worker_sha256":expected,
            "journal_format":11,"preparation":aq_admission::preparation_refresh_v1::contract_artifact().unwrap(),
            "terminal":aq_phase_terminal_protocol::refresh_v1::contract_artifact().unwrap(),
            "actual_host_pins":host,
        }),
    );
    let qualification = publish(
        &mut profiled,
        "aq.static-worker-qualification/0.4.0-refresh-output-v1",
        json!({
            "scope":"explicit synthetic/static integration fixture; production capabilities remain unqualified",
            "runtime":runtime,"worker_sha256":expected,"grant_deadline_semantics":"original issued limits only",
        }),
    );
    profiled.qualified_runtimes[0].runtime = runtime;
    profiled.qualified_runtimes[0].qualification = qualification;
    profiled.revision += 1;
    profiled.validate().unwrap();
    config.profiled_execution = Some(Arc::new(profiled));
    case.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
}
