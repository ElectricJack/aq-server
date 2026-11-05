//! Explicit disposable actual Codex/finite-peer task integration. No external
//! provider, canonical data, or universal runtime/model qualification.
#[allow(dead_code)] // Shared Git helper has APIs used by the source-retention fixture.
#[path = "support/actual_git_owner.rs"]
mod actual_git_owner;
#[path = "support/codex_finite_provider.rs"]
mod codex_finite_provider;
#[path = "support/codex_operator_assessment.rs"]
mod codex_operator_assessment;
#[path = "support/codex_runner_process.rs"]
mod codex_runner_process;
#[path = "support/codex_runtime_recipe.rs"]
mod codex_runtime_recipe;
#[path = "support/codex_terminal_output.rs"]
mod codex_terminal_output;
mod common;
#[path = "support/native_result_settlement.rs"]
mod native_result_settlement;
#[path = "support/native_runner_transport.rs"]
mod native_runner_transport;
#[allow(dead_code)] // Shared Graph helper also serves its standalone component proof.
#[path = "support/real_graph.rs"]
mod real_graph;
use anyhow::{Context, Result, ensure};
use aq_admission::{ArtifactRef, Digest, ObjectFormat, ObjectId, Text};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
const STATE: &str = "/var/lib/aq-qualification/state";
const MINIMUM_FREE: u64 = 11_005_853_696;
const WRAPPER_SHA: &str = "3a97e63fb2d772487e2e3e51f2e9b27bdb229d8630489ede299599cb30040cb6";
const BOOTSTRAP_SHA: &str = "a11dbd3ff50a3e45b896ac1e3df8d51729c4bc9cf57eecadad992ec5f3f56dd0";
const TOOL_SHA: &str = "ef5788e7b96098fbac79e8e1ad28d7b37db7446eea335fe38f480e24fcaeeaf5";
fn headroom() -> Result<u64> {
    let mut value: nix::libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { nix::libc::statvfs(c"/tmp".as_ptr(), &mut value) } == 0,
        "host disk observation"
    );
    let bytes = value.f_bavail.saturating_mul(value.f_frsize);
    ensure!(bytes > MINIMUM_FREE, "unchanged Git minimum unavailable");
    Ok(bytes)
}
fn digest(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest as _, Sha256};
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hash.update(&bytes[..n]);
    }
    Ok((format!("{:x}", hash.finalize()), size))
}
fn private(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
struct Pins {
    images: codex_runtime_recipe::ImagePins,
    outer: BTreeMap<String, PathBuf>,
    worker: PathBuf,
    wrapper: PathBuf,
}
impl Pins {
    fn load() -> Result<Self> {
        fn image(
            variable: &str,
            id: &str,
            guest: &str,
        ) -> Result<(codex_runtime_recipe::ProgramPin, PathBuf)> {
            let path = PathBuf::from(std::env::var(variable)?);
            ensure!(path.is_absolute(), "absolute reviewed image path");
            let expected =
                std::env::var(format!("{}_SHA256", variable.trim_end_matches("_BINARY")))?;
            let (actual, bytes) = digest(&path)?;
            ensure!(actual == expected, "actual selected image changed");
            Ok((
                codex_runtime_recipe::ProgramPin {
                    path: PathBuf::from(guest),
                    artifact: ArtifactRef {
                        id: Text::new(id)?,
                        version: Text::new("1")?,
                        digest: serde_json::from_value(json!(actual))?,
                    },
                    bytes,
                },
                path,
            ))
        }
        let (worker, worker_path) = image(
            "AQ_GATE04_WORKER_BINARY",
            "aq.worker-native-code",
            "/opt/aq-qualification/aq-worker",
        )?;
        let (codex, codex_path) = image(
            "AQ_NATIVE_CODEX_BINARY",
            "aq.codex-0.166-image",
            "/opt/aq-qualification/codex",
        )?;
        let (helper, helper_path) = image(
            "AQ_NATIVE_HOST_BINARY",
            "aq.session-host-image",
            "/opt/aq-qualification/aq-session-host",
        )?;
        let (tmux, tmux_path) = image(
            "AQ_NATIVE_TMUX_BINARY",
            "aq.tmux-image",
            "/opt/aq-qualification/tmux",
        )?;
        let (client, client_path) = image(
            "AQ_NATIVE_AGENT_CLIENT_BINARY",
            "aq.agent-result-client-image",
            "/opt/aq-qualification/aq-agent",
        )?;
        let wrapper = PathBuf::from(std::env::var("AQ_GATE04_WRAPPER")?);
        let bootstrap = wrapper
            .parent()
            .context("wrapper parent absent")?
            .join("native-runner-fixture.py");
        ensure!(
            digest(&wrapper)?.0 == WRAPPER_SHA && digest(&bootstrap)?.0 == BOOTSTRAP_SHA,
            "reviewed wrapper/bootstrap source changed"
        );
        let images = codex_runtime_recipe::ImagePins {
            worker,
            worker_source: std::env::var("AQ_GATE04_WORKER_SOURCE")?,
            server_source: std::env::var("AQ_GATE04_SERVER_SOURCE")?,
            codex,
            helper,
            tmux,
            client,
            result_recipe: include_str!("fixtures/codex-result-local-recipe-v1.json").into(),
            startup: include_str!("fixtures/agent-result-startup-v1.md").into(),
            contract: std::fs::read_to_string(
                "/tmp/aq-agent-result-protocol-pin-v1/contract.json",
            )?,
        };
        Ok(Self {
            images,
            outer: BTreeMap::from([
                ("AQ_NATIVE_CODEX_BINARY".into(), codex_path),
                ("AQ_NATIVE_HOST_BINARY".into(), helper_path),
                ("AQ_NATIVE_TMUX_BINARY".into(), tmux_path),
                ("AQ_NATIVE_AGENT_CLIENT_BINARY".into(), client_path),
            ]),
            worker: worker_path,
            wrapper,
        })
    }
    fn environment(&self, socket: &Path) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        env.insert(
            "AQ_NATIVE_WORKER_BINARY".into(),
            self.worker.to_str().context("worker path UTF8")?.into(),
        );
        env.insert(
            "AQ_NATIVE_WORKER_SHA256".into(),
            self.images.worker.artifact.digest.as_str().into(),
        );
        env.insert(
            "AQ_NATIVE_FIXTURE_CONTROL_SOCKET".into(),
            socket.to_str().context("fixture socket UTF8")?.into(),
        );
        for (name, path) in &self.outer {
            env.insert(
                name.clone(),
                path.to_str().context("image path UTF8")?.into(),
            );
            env.insert(
                format!("{}_SHA256", name.trim_end_matches("_BINARY")),
                digest(path)?.0,
            );
        }
        env.insert(
            "AQ_NATIVE_GIT_BINARY".into(),
            std::env::var("AQ_TEST_GIT_CANDIDATE")?,
        );
        env.insert(
            "AQ_NATIVE_GIT_SHA256".into(),
            std::env::var("AQ_TEST_GIT_CANDIDATE_SHA256")?,
        );
        Ok(env)
    }
}
fn git_object(object: &ObjectId) -> Result<aq_worker_protocol::ObjectId> {
    ensure!(
        object.format == ObjectFormat::Sha1,
        "actual Git profile requires SHA1"
    );
    let wire = aq_worker_protocol::ObjectId {
        algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
        hex: object.hex.clone(),
    };
    aq_worker_protocol::Validate::validate(&wire).map_err(anyhow::Error::msg)?;
    Ok(wire)
}
#[test]
fn actual_git_rpc_uses_closed_algorithm_wire_without_changing_source_identity() {
    let base = ObjectId {
        format: ObjectFormat::Sha1,
        hex: "a".repeat(40),
    };
    let wire = git_object(&base).unwrap();
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        json!({"algorithm":"sha1","hex":base.hex})
    );
    assert!(serde_json::from_value::<aq_worker_protocol::ObjectId>(json!(base)).is_err());
}
struct Source {
    central_binding: Value,
    provisioning_operation: Uuid,
    owner: actual_git_owner::Owner,
    base: ObjectId,
    retention: Uuid,
    configuration: aq_server::source_retention::SourceRetentionConfig,
}
async fn source(root: &Path, repository: Uuid) -> Result<Source> {
    actual_git_owner::verify_pins();
    headroom()?;
    let seed = root.join("seed");
    std::fs::create_dir(&seed)?;
    actual_git_owner::git(&seed, &["init", "-b", "main"]).await;
    private(
        &seed.join("README.md"),
        b"Disposable AQ finite Codex task fixture.\n",
        0o600,
    )?;
    actual_git_owner::git(&seed, &["add", "."]).await;
    actual_git_owner::git(&seed, &["commit", "-m", "fixture base"]).await;
    let base = ObjectId {
        format: ObjectFormat::Sha1,
        hex: actual_git_owner::git(&seed, &["rev-parse", "HEAD"]).await,
    };
    let wire = git_object(&base)?;
    let owner = actual_git_owner::Owner::start(root, "central-git").await;
    let central_binding = owner.call(repository,"repo.register",json!({"identity":"aq-gate04-actual-source","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    owner.call(repository, "repo.sync", json!({})).await;
    let workspace = owner
        .call(repository, "workspace.create", json!({"base":wire}))
        .await;
    let checkpoint=owner.call(repository,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":wire,"quiescent":true,"mode":"head"})).await;
    let retention = serde_json::from_value(checkpoint["retention_handle"].clone())?;
    let configuration = aq_server::source_retention::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        git_root: owner.root.clone(),
        git_socket: owner.socket.clone(),
        staging_root: root.join("central-staging"),
        max_staging_bytes: 8 * 1024 * 1024,
        max_bundle_bytes: 1024 * 1024,
        bulk_concurrency: 1,
        incomplete_ttl_secs: 60,
    };
    Ok(Source {
        central_binding,
        provisioning_operation: Uuid::new_v4(),
        owner,
        base,
        retention,
        configuration,
    })
}
async fn install(
    actual: &mut common::selection::ActualSelectionFixture,
    recipe: &codex_runtime_recipe::Prepared,
    key: &Path,
) -> Result<()> {
    let mut config = (*actual.env.state.config).clone();
    config.intelligence = Some(Arc::new(
        recipe
            .registry(actual.env.node, key.into())?
            .load()
            .map_err(anyhow::Error::msg)?,
    ));
    config.profiled_execution = Some(Arc::new(recipe.profiled.clone()));
    let terminal_key = actual.env._dir.path().join("terminal-key");
    private(&terminal_key, &[71u8; 32], 0o600)?;
    use aq_server::terminal::{
        config::Loaded,
        protection::{KeyFile, Keyring, ProtectionConfig},
    };
    config.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "finite-codex".into(),
            keys: vec![KeyFile {
                id: "finite-codex".into(),
                file: terminal_key,
            }],
        })?,
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    actual.env.state.config = Arc::new(config);
    let (status, _) = common::selection::call(
        &aq_server::build_admin_router(actual.env.state.clone()),
        "/api/intelligence/register",
        Some(json!({})),
        "owner@localhost",
    )
    .await;
    ensure!(status == 200, "actual protected registry install refused");
    Box::pin(aq_server::profiled_execution::policies::install(
        &actual.env.state,
    ))
    .await
    .map_err(|_| anyhow::anyhow!("actual execution policy install refused"))?;
    Box::pin(aq_server::terminal::persistence::install(&actual.env.state))
        .await
        .map_err(|_| anyhow::anyhow!("actual terminal owner install refused"))?;
    Ok(())
}
fn journal(private: &native_runner_transport::PrivateState, node: Uuid) -> Result<Value> {
    let value: Value =
        serde_json::from_slice(&private.read(Path::new("journal/journal.json"), 4 * 1024 * 1024)?)?;
    let mut state = value["state"].clone();
    state.sort_all_objects();
    ensure!(
        json!(Digest::of(&serde_json::to_vec(&state)?)) == value["digest"]
            && state["format"] == 12
            && state["identity"]["node_id"] == node.to_string(),
        "actual current journal identity/digest"
    );
    Ok(state)
}

struct ControlOwners {
    state: aq_server::AppState,
    pool: sqlx::PgPool,
    node: Uuid,
    recipe: Arc<codex_runtime_recipe::Prepared>,
    transfer: Arc<native_runner_transport::Transferred>,
    evidence: codex_operator_assessment::LocalEvidence,
    listener: std::net::TcpListener,
    provider: Arc<tokio::task::JoinHandle<()>>,
    assessor: tokio::sync::Mutex<codex_operator_assessment::Assessor>,
    observed: codex_terminal_output::Observed,
}
async fn assessed_request(
    request: axum::extract::Request,
    owners: &ControlOwners,
) -> Result<axum::extract::Request> {
    use codex_operator_assessment::{PeerWitness, Stage};
    let witness = PeerWitness {
        listener: &owners.listener,
        server: &owners.provider,
        origin: &owners.transfer.info.provider_origin,
    };
    match request.uri().path() {
        "/v1/worker/profiled/poll" => {
            Box::pin(owners.assessor.lock().await.assess(
                &owners.state,
                &owners.pool,
                owners.node,
                &owners.recipe,
                &owners.transfer.state,
                &owners.evidence,
                witness,
                Stage::BeforePoll,
            ))
            .await?;
            Ok(request)
        }
        "/v1/worker/profiled-v2/prepare" => {
            let (parts, body) = request.into_parts();
            let bytes =
                axum::body::to_bytes(body, aq_admission::preparation_refresh_v1::MAX_BYTES).await?;
            let document = aq_admission::preparation_refresh_v1::decode(&bytes)?;
            let aq_admission::preparation_refresh_v1::Document::Prepare(prepared) = document else {
                anyhow::bail!("actual refresh Prepare document required")
            };
            Box::pin(owners.assessor.lock().await.assess(
                &owners.state,
                &owners.pool,
                owners.node,
                &owners.recipe,
                &owners.transfer.state,
                &owners.evidence,
                witness,
                Stage::BeforePrepare(&prepared),
            ))
            .await?;
            Ok(axum::extract::Request::from_parts(
                parts,
                axum::body::Body::from(bytes),
            ))
        }
        _ => Ok(request),
    }
}
fn control_router(owners: Arc<ControlOwners>) -> axum::Router {
    aq_server::build_worker_router(owners.state.clone()).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let owners = owners.clone();
            Box::pin(async move {
                match assessed_request(request, &owners).await {
                    Ok(request) => owners.observed.inspect(request, next).await,
                    Err(error) => {
                        let code = codex_operator_assessment::refusal_code(&error);
                        owners
                            .observed
                            .statuses
                            .lock()
                            .unwrap()
                            .push((format!("fixture_operator:{code}"), 503));
                        eprintln!("GATE04_OPERATOR_REFUSAL {}",json!({"code":code,"clocks":codex_operator_assessment::refusal_clocks(&error)}));
                        axum::response::Response::builder()
                            .status(503)
                            .body(axum::body::Body::from(
                                "finite fixture operator assessment refused",
                            ))
                            .unwrap()
                    }
                }
            })
        },
    ))
}
fn configure(
    actual: &common::selection::ActualSelectionFixture,
    recipe: &codex_runtime_recipe::Prepared,
    origin: &str,
) -> Result<Value> {
    #[derive(serde::Serialize)]
    struct NodeConfig<'a> {
        server_url: &'a str,
        node_id: Uuid,
        credential: &'a str,
    }
    let worker = toml::to_string(&NodeConfig {
        server_url: origin,
        node_id: actual.env.node,
        credential: &actual.env.credential,
    })?;
    let files = recipe.files()?;
    let file = |name: &str| -> Result<Value> {
        Ok(serde_json::from_str(
            files
                .get(name)
                .context("fixed recipe configuration absent")?,
        )?)
    };
    Ok(
        json!({"schema_version":1,"operation":"configure","worker_config":worker,"native_config":recipe.native_json(Path::new(STATE))?,
        "installation_manifest":file("adapter.json")?,"inventory_definition":file("inventory.json")?,"client_manifest":file("client.json")?,"composite_definition":file("composite.json")?}),
    )
}
async fn running_watch(
    actual: &common::selection::ActualSelectionFixture,
    transfer: &native_runner_transport::Transferred,
    worker: &mut codex_runner_process::RunnerProcess,
) -> Result<(codex_terminal_output::Watched, Value, u64, PathBuf, String)> {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            ensure!(!worker.finished()?, "ordinary runner exited before Watch");
            if let Ok(state) = journal(&transfer.state, actual.env.node)
                && let Some(attempts) = state["attempts"].as_object()
            {
                ensure!(attempts.len() <= 1, "one actual attempt required");
                if let Some(attempt) = attempts.values().next()
                    && let Some(deadline) = attempt["refresh_execution"]["deadline_ns"].as_u64()
                {
                    let mut now: nix::libc::timespec = unsafe { std::mem::zeroed() };
                    ensure!(
                        unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_BOOTTIME, &mut now) }
                            == 0,
                        "original boot clock"
                    );
                    ensure!(
                        (now.tv_sec as u64) * 1_000_000_000 + (now.tv_nsec as u64) < deadline,
                        "original grant expired before Watch"
                    );
                    if attempt["native_host"]["refresh_terminal"]["started"]["receipt"].is_object()
                    {
                        let terminal: Option<Uuid> = sqlx::query_scalar(
                            "SELECT terminal_id FROM terminal_sessions WHERE node_id=$1",
                        )
                        .bind(actual.env.node)
                        .fetch_optional(&actual.env.pool)
                        .await?;
                        if let Some(terminal) = terminal {
                            let watched = codex_terminal_output::watch_registered(
                                &actual.env.state,
                                terminal,
                            )
                            .await?;
                            let root = PathBuf::from(
                                attempt["result"]["scope"]["root"]
                                    .as_str()
                                    .context("actual result channel root")?,
                            );
                            let relative = root.strip_prefix(STATE)?.join("binding.json");
                            let binding: Value =
                                serde_json::from_slice(&transfer.state.read(&relative, 16384)?)?;
                            ensure!(
                                binding == attempt["result"]["scope"]["binding"],
                                "actual published result binding changed"
                            );
                            let launch = attempt["native_host"]["running"]["request"]["launch_id"]
                                .as_str()
                                .context("actual launch ID")?;
                            Uuid::parse_str(launch)?;
                            let auth = PathBuf::from("host")
                                .join(launch)
                                .join("codex-home/auth.json");
                            let auth_value: Value =
                                serde_json::from_slice(&transfer.state.read(&auth, 8192)?)?;
                            let cap = auth_value["OPENAI_API_KEY"]
                                .as_str()
                                .context("actual scoped capability")?
                                .to_owned();
                            ensure!(!cap.is_empty(), "scoped capability empty");
                            return Ok::<_, anyhow::Error>((watched, binding, deadline, auth, cap));
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("actual prelaunch/Watch outer bound")?
}
async fn model_input_evidence(
    actual: &common::selection::ActualSelectionFixture,
    attempt: &Value,
    requests: &[Value],
) -> Result<Value> {
    let offered: aq_admission::startup::NativeOffer =
        serde_json::from_value(attempt["reservation"]["offer"].clone())?;
    let payload = &offered.startup.payload;
    let descriptor = offered
        .offered
        .artifacts
        .iter()
        .find(|a| a.artifact == *payload)
        .context("actual sealed payload descriptor")?;
    let id = Uuid::parse_str(
        descriptor
            .retention_receipt
            .id
            .as_str()
            .strip_prefix("aq.inline-retention/")
            .context("actual payload retention identity")?,
    )?;
    let mut tx = actual.env.pool.begin().await?;
    let retained = aq_server::profiled_execution::artifacts::load_locked(&mut tx, id)
        .await
        .map_err(|_| anyhow::anyhow!("actual payload owner read refused"))?;
    ensure!(
        retained.project_id == actual.project.binding.project_id
            && serde_json::to_value(&retained.descriptor)? == serde_json::to_value(descriptor)?,
        "actual sealed payload owner mismatch"
    );
    offered.startup.verify_payload(&retained.bytes)?;
    tx.commit().await?;
    let text = std::str::from_utf8(&retained.bytes)?;
    let startup = include_str!("fixtures/agent-result-startup-v1.md");
    ensure!(
        text.matches(startup).count() == 1,
        "reporting startup must be counted exactly once"
    );
    fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(s) => out.push(s),
            Value::Array(a) => {
                for v in a {
                    strings(v, out)
                }
            }
            Value::Object(a) => {
                for v in a.values() {
                    strings(v, out)
                }
            }
            _ => {}
        }
    }
    let first = requests.first().context("actual model request absent")?;
    let mut input = Vec::new();
    strings(&first["input"], &mut input);
    ensure!(
        input.iter().map(|s| s.matches(text).count()).sum::<usize>() == 1,
        "actual Codex input must contain exact sealed payload once"
    );
    ensure!(
        input
            .iter()
            .map(|s| s.matches(startup).count())
            .sum::<usize>()
            == 1,
        "actual Codex input must contain reporting startup once"
    );
    let input_bytes = serde_json::to_vec(&first["input"])?.len();
    let tool_bytes = serde_json::to_vec(&first["tools"])?.len();
    // Include the entire canonical request (instructions and metadata too),
    // conservatively charging JSON overhead outside the raw sealed payload.
    let first_bytes = serde_json::to_vec(first)?.len();
    let implicit_units = first_bytes
        .checked_sub(retained.bytes.len())
        .context("finite input measurement underflow")?;
    ensure!(
        (implicit_units as u64) <= offered.startup.implicit_tokens_upper_bound,
        "actual finite implicit/tool bytes exceed declared conservative planning allowance"
    );
    Ok(
        json!({"model":first["model"],"max_output_tokens":first["max_output_tokens"],"sealed_payload_bytes":retained.bytes.len(),"sealed_payload_digest":payload.digest,
        "public_reporting_bytes":startup.len(),"public_reporting_digest":Digest::of(startup.as_bytes()),"payload_occurrences":1,"reporting_occurrences":1,
        "canonical_first_request_bytes":first_bytes,"canonical_first_input_encoded_bytes":input_bytes,"canonical_tool_definition_bytes":tool_bytes,"finite_implicit_tool_planning_units":implicit_units,"declared_implicit_planning_units":offered.startup.implicit_tokens_upper_bound,"actual_tools":first["tools"].as_array().context("actual tools missing")?.iter().map(|t|json!({"type":t["type"],"name":t["name"]})).collect::<Vec<_>>(),
        "canonical_request_metadata":requests.iter().map(|r|serde_json::to_vec(r).map(|b|json!({"bytes":b.len(),"digest":Digest::of(&b)}))).collect::<std::result::Result<Vec<_>,_>>()?,
        "scope":"measured finite-peer wire/input only; no external tokenizer/window/provider qualification"}),
    )
}
fn diagnostics(
    transfer: &native_runner_transport::Transferred,
    node: Uuid,
    observed: &codex_terminal_output::Observed,
) -> Value {
    let state = journal(&transfer.state, node).ok();
    let attempt = state
        .as_ref()
        .and_then(|v| v["attempts"].as_object())
        .and_then(|v| v.values().next());
    let field = |key: &str| attempt.map(|a| a[key].is_object()).unwrap_or(false);
    let host = attempt.map(|a| &a["native_host"]);
    json!({"journal_format":state.as_ref().map(|s|&s["format"]),"journal_revision":state.as_ref().map(|s|&s["revision"]),
        "source_progress":attempt.map(|a| {let s=&a["source"]; json!({
            "plan":s.is_object(),"manifest":s["manifest"].is_object(),"verified":s["verified"].is_object(),
            "manifest_offset":s["manifest_offset"].as_u64(),"bundle_offset":s["bundle_offset"].as_u64(),
            "operations":(["repository","import","create","retention","workspace"].map(|key| json!({"stage":key,
                "intent":s["operations"][key].is_object(),"result":s["operations"][key]["result"].is_object()})))})}),
        "accepted":field("refresh_acceptance"),"prepared_inputs":field("refresh_prepared"),"execution":field("refresh_execution"),
        "original_execution_deadline_boot_ns":attempt.and_then(|a|a["refresh_execution"]["deadline_ns"].as_u64()),
        "original_reservation_deadline_boot_ns":attempt.and_then(|a|a["reservation"]["deadline_ns"].as_u64()),
        "host_intent":host.is_some_and(|h|h["intent"].is_object()),"physical_intent":host.is_some_and(|h|h["host_intent"].is_object()),"guardian":host.is_some_and(|h|h["guardian"].is_object()),"start_intended":host.and_then(|h|h["start_intended"].as_bool()),"result_history":field("result"),
        "running":host.is_some_and(|h|h["running"].is_object()),"stopped":host.is_some_and(|h|h["stopped"].is_object()),
        "kernel_wait_echild":host.and_then(|h|h["stopped"]["observation"]["kernel_wait_echild"].as_bool()),
        "started_ack":host.is_some_and(|h|h["refresh_terminal"]["started"]["receipt"].is_object()),
        "stopped_ack":host.is_some_and(|h|h["refresh_terminal"]["stopped"]["receipt"].is_object()),
        "local_claim":attempt.is_some_and(|a|a["result"]["claim"].is_object()),"source_receipt":attempt.is_some_and(|a|a["result"]["source"]["receipt"].is_object()),
        "b1_receipt":attempt.is_some_and(|a|a["result"]["receipt"].is_object()),"http_statuses":*observed.statuses.lock().unwrap(),
        "accepted_output_exchanges":observed.uploads.lock().unwrap().len(),"fixture_sender_exited":transfer.sender_exited().ok()})
}
#[allow(clippy::too_many_arguments)] // Explicit existing fixture owners and exact synthetic material; no new authority facade.
async fn execute_case(
    root: &Path,
    pins: &Pins,
    actual: &mut common::selection::ActualSelectionFixture,
    recipe: Arc<codex_runtime_recipe::Prepared>,
    source: &Source,
    graph: &real_graph::RealGraphFixture,
    key_path: &Path,
    key: &str,
) -> Result<Value> {
    use codex_finite_provider::{FinitePeer, FixtureCli, ToolProgram};
    let tools = root.join("codex_tool_commands.py");
    let tool_bytes = include_bytes!("support/codex_tool_commands.py");
    ensure!(
        Digest::of(tool_bytes).as_str() == TOOL_SHA,
        "frozen tool program changed"
    );
    private(&tools, tool_bytes, 0o500)?;
    let program = ToolProgram { path: tools };
    let evidence = codex_operator_assessment::LocalEvidence {
        images: vec![
            codex_operator_assessment::LocalImage {
                guest: pins.images.worker.clone(),
                outer: pins.worker.clone(),
            },
            codex_operator_assessment::LocalImage {
                guest: pins.images.codex.clone(),
                outer: pins.outer["AQ_NATIVE_CODEX_BINARY"].clone(),
            },
            codex_operator_assessment::LocalImage {
                guest: pins.images.helper.clone(),
                outer: pins.outer["AQ_NATIVE_HOST_BINARY"].clone(),
            },
            codex_operator_assessment::LocalImage {
                guest: pins.images.tmux.clone(),
                outer: pins.outer["AQ_NATIVE_TMUX_BINARY"].clone(),
            },
            codex_operator_assessment::LocalImage {
                guest: pins.images.client.clone(),
                outer: pins.outer["AQ_NATIVE_AGENT_CLIENT_BINARY"].clone(),
            },
        ],
        synthetic_key_file: key_path.into(),
        synthetic_key_digest: Digest::of(key.as_bytes()),
        synthetic_key_bytes: key.len() as u64,
    };
    // Hash immutable images before the worker exists. Live callbacks retain
    // these real descriptors and only revalidate continuity in O(1).
    let assessor = codex_operator_assessment::Assessor::seal_images(&recipe, &evidence)?;
    let peer = Arc::new(
        Box::pin(FinitePeer::new(
            FixtureCli {
                path: pins.worker.clone(),
            },
            program.clone(),
            codex_runtime_recipe::MODEL.into(),
            key.into(),
            vec![actual.env.credential.clone()],
        ))
        .await?,
    );
    let socket = root.join("fixture.sock");
    let listener = native_runner_transport::ControlListener::bind(&socket)?;
    let mut worker = codex_runner_process::RunnerProcess::start(
        &pins.wrapper,
        &pins.environment(&socket)?,
        &root.join("worker.stderr"),
    )?;
    let pid = worker.pid()?;
    let transferred = tokio::task::spawn_blocking(move || listener.receive(pid)).await??;
    let packet = configure(actual, &recipe, &transferred.info.control_origin)?;
    let transferred = Arc::new(transferred.configure(&packet)?);
    let provider_listener = transferred.provider.try_clone()?;
    let provider_tcp = tokio::net::TcpListener::from_std(transferred.provider.try_clone()?)?;
    let provider_router = peer.router.clone();
    let provider = Arc::new(tokio::spawn(async move {
        axum::serve(provider_tcp, provider_router).await.unwrap()
    }));
    let observed = codex_terminal_output::Observed::default();
    let owners = Arc::new(ControlOwners {
        state: actual.env.state.clone(),
        pool: actual.env.pool.clone(),
        node: actual.env.node,
        recipe: recipe.clone(),
        transfer: transferred.clone(),
        evidence,
        listener: provider_listener,
        provider: provider.clone(),
        assessor: tokio::sync::Mutex::new(assessor),
        observed: observed.clone(),
    });
    let control_tcp = tokio::net::TcpListener::from_std(transferred.control.try_clone()?)?;
    let router = control_router(owners);
    let control = tokio::spawn(async move { axum::serve(control_tcp, router).await.unwrap() });
    let result=Box::pin(async {
        eprintln!("GATE04_STAGE preflight");
        let ready=worker.preflight().await?;
        ensure!(ready["runtime"]==json!(recipe.runtime)&&ready["qualification"]==json!(recipe.qualification)&&ready["adapter"]==json!(recipe.adapter),"genuine selected preflight pins differ");
        eprintln!("GATE04_STAGE worker_repository_provisioning");
        let state_owner=transferred.clone();let central=source.central_binding.clone();let operation=source.provisioning_operation;
        let repository=tokio::task::spawn_blocking(move||state_owner.state.provision_worker_repository(&central,operation)).await??;
        ensure!(repository["current"]["repository_id"]==json!(actual.project.binding.repository_id)
            &&repository["current"]["binding_generation"]==actual.project.binding.repository_binding_generation,"actual project repository provisioning mismatch");
        private(&root.join("worker-repository-provisioning.json"),&serde_json::to_vec(&repository)?,0o600)?;
        headroom()?;ensure!(transferred.state.free_bytes()? >= recipe.source_disk_bytes,"actual private source reservation unavailable");eprintln!("GATE04_STAGE ordinary_run_once");worker.continue_once().await?;
        let (watched,binding,deadline,auth,cap)=Box::pin(running_watch(actual,&transferred,&mut worker)).await?;
        ensure!(cap!=key && cap!=actual.env.credential,"fresh scoped capability required");
        peer.release_after_watch(binding.clone(),deadline,cap)?;
        let status=worker.wait().await.context("gate04_worker_wait")?;ensure!(status.success(),"ordinary RunNative failed");
        let state=journal(&transferred.state,actual.env.node)?;
        let attempts=state["attempts"].as_object().context("actual attempts absent")?;ensure!(attempts.len()==1,"one retained actual attempt");
        let attempt=attempts.values().next().unwrap();let host=&attempt["native_host"];
        ensure!(host["running"].is_object() && host["stopped"]["observation"]["kernel_wait_echild"]==true &&host["refresh_terminal"]["started"]["receipt"].is_object()&&host["refresh_terminal"]["stopped"]["receipt"].is_object(),"actual physical stop/paired ACK required");
        ensure!(transferred.state.missing(&auth)? &&transferred.sender_exited()?,"actual private auth/fixture sender cleanup");
        let tools=peer.tool_validation(&program,&binding).await.context("gate04_tool_validation")?;
        let requests=peer.requests.lock().unwrap().clone();
        let model_input=model_input_evidence(actual,attempt,&requests).await.context("gate04_model_input")?;
        let workspace=Path::new(attempt["result"]["scope"]["workspace"]["path"].as_str().context("actual retained worktree")?).strip_prefix(STATE)?;
        let proof=transferred.state.read(&workspace.join("gate04-proof.txt"),128)?;
        ensure!(proof==b"AQ native result fixture proof\n","actual fixed file differs");
        ensure!(attempt["result"]["receipt"].is_object(),"actual B1 receipt required");
        let terminal=codex_terminal_output::verify_tail(&actual.env.state,&watched,&observed).await.context("gate04_terminal_tail")?;
        eprintln!("GATE04_STAGE protected_result_settlement");
        let report=serde_json::from_value(binding["report_id"].clone())?;
        let source_operation=serde_json::from_value(attempt["result"]["report"]["request"]["source_receipt_ids"][0].clone())?;
        let review_key=root.join("review-key");private(&review_key,&[93u8;32],0o600)?;
        let keys=Arc::new(aq_server::protected_storage::Keyring::load(&aq_server::protected_storage::ProtectionConfig {active_key_id:"finite-gate04".into(),keys:vec![aq_server::protected_storage::KeyFile {id:"finite-gate04".into(),file:review_key} ]})?);
        let reviews=aq_server::reviews::Service::new(actual.env.state.clone(),keys);
        let ids=native_result_settlement::StableIds::capture(&actual.env.state)?;
        let question=Box::pin(native_result_settlement::prepare_question(&actual.env.state,&reviews,&source.owner,native_result_settlement::B1Request {pool:&actual.env.pool,actor:actual.actor,report_id:report,source_operation,original_task:&actual.task,ids:&ids},native_result_settlement::Observations {original_local_binding:&binding,validated_tool_output:&tools,worktree_member:"gate04-proof.txt",private_file_bytes:&proof})).await?;
        let settled=Box::pin(native_result_settlement::finish_fixture_choice(&actual.env.state,&reviews,&question,native_result_settlement::FixtureOwnerDecision::ApproveAfterExactProof)).await?;
        ensure!(settled.completion.state=="confirmed" &&settled.completion.receipt.is_some(),"actual Graph completion remains held");
        let task=graph.task().await?;ensure!(task["task_state"]=="completed" &&task["version"]==json!(actual.task.version+1)&&task["content_revision"]==json!(actual.task.content_revision+1),"independent actual Graph task closure differs");
        Ok::<_,anyhow::Error>(json!({"scope":"actual pinned Codex finite local peer only; simulated fixture Owner approval","runtime":recipe.runtime,"qualification":recipe.qualification,"execution_deadline_boot_ns":deadline,"request_count":peer.requests.lock().unwrap().len(),"model_input":model_input,"tool_validation":tools,"terminal":terminal,"physical_running":true,"physical_stopped_echild":true,"started_ack":true,"stopped_ack":true,"private_auth_removed":true,"b1_receipt":attempt["result"]["receipt"],"settlement":settled,"actual_graph_task":task}))
    }).await;
    if result.is_err() {
        // Private forensic bytes remain outside source control and stdout. They
        // are history only, never reopened as launch or current-state authority.
        if let Ok(bytes) = transferred
            .state
            .read(Path::new("journal/journal.json"), 4 * 1024 * 1024)
        {
            let _ = private(&root.join("refused-journal.json"), &bytes, 0o600);
        }
        // Retain bounded private observations before the PID/mount view closes.
        // These bytes support offline fixture assertions, never live authority.
        let requests = peer.requests.lock().unwrap().clone();
        if let Ok(bytes) = serde_json::to_vec(&requests) {
            let _ = private(&root.join("refused-provider-requests.json"), &bytes, 0o600);
        }
        if let Ok(state) = journal(&transferred.state, actual.env.node)
            && let Some(attempt) = state["attempts"]
                .as_object()
                .and_then(|a| a.values().next())
            && let Some(path) = attempt["result"]["scope"]["workspace"]["path"].as_str()
            && let Ok(relative) = Path::new(path).strip_prefix(STATE)
            && let Ok(bytes) = transferred
                .state
                .read(&relative.join("gate04-proof.txt"), 128)
        {
            let _ = private(&root.join("refused-proof-bytes"), &bytes, 0o600);
        }
        let metadata = diagnostics(&transferred, actual.env.node, &observed);
        eprintln!("GATE04_DIAGNOSTICS {metadata}");
        let _ = private(
            &root.join("refused-metadata.json"),
            &serde_json::to_vec_pretty(&metadata).unwrap_or_default(),
            0o600,
        );
    }
    let cleanup = worker.cleanup().await;
    control.abort();
    provider.abort();
    cleanup.context("wrapper cleanup uncertain")?;
    result
}
async fn actual_case() -> Result<Value> {
    ensure!(
        unsafe { nix::libc::getsid(0) } == unsafe { nix::libc::getpid() },
        "run the pinned test binary in a fresh owned SID"
    );
    let free = headroom()?;
    let pins = Pins::load()?;
    ensure!(
        digest(Path::new("/usr/bin/bwrap"))?.0
            == "e318903862396f96de3df57264e0158682b952fd3fb53ac23d876413e7b30f71",
        "actual selected sandbox executable changed"
    );
    let root = tempfile::Builder::new().prefix("aq-g04-").tempdir()?.keep();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    // Retained before any child; uncertainty never deletes a live owner's root.
    eprintln!("GATE04_PRIVATE_ROOT {}", root.display());
    let project = Uuid::new_v4();
    let repository = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    let task_id = Uuid::new_v4();
    let graph=Box::pin(real_graph::RealGraphFixture::start(real_graph::BinaryPin{path:std::env::var("AQ_GATE04_GRAPH_BINARY")?.into(),sha256:std::env::var("AQ_GATE04_GRAPH_SHA256")?},real_graph::TaskSeed {workspace_id:workspace,task_id,title:"Disposable finite Codex source task".into(),body:"Create only gate04-proof.txt containing exactly AQ native result fixture proof and a newline. Compare exact bytes, then submit a factual local AQ result claim using the counted instructions. This is a finite local Responses integration, not an external model or real human review.".into(),acceptance_criteria:"Fixed source file is retained centrally; actual tool cmp passes; scoped claim is acknowledged; protected simulated fixture review precedes actual task CAS.".into()})).await?;
    let mut source = Box::pin(source(&root, repository)).await?;
    let result = Box::pin(async {
        let catalog =
            codex_runtime_recipe::catalog(project, repository).context("gate04_catalog")?;
        let binding = codex_runtime_recipe::project(project, repository, 1, workspace, &catalog)
            .context("gate04_project")?;
        let pin = graph.task_pin();
        let task = aq_admission::TaskRevision {
            workspace_id: workspace,
            task_id,
            version: pin.version.try_into()?,
            content_revision: pin.content_revision.try_into()?,
        };
        let setup = common::selection::ActualSetup {
            enrollment_worker_version: "0.5.1".into(),
            project: binding,
            catalog_artifact_json: String::from_utf8(catalog.canonical_bytes()?)?,
            graph: graph.agent_access_config(),
            task,
            source: source.configuration.clone(),
        };
        let mut actual = Box::pin(common::native_offer::setup_actual(&setup))
            .await
            .context("gate04_actual_setup")?;
        let recipe = Arc::new(
            codex_runtime_recipe::prepare(codex_runtime_recipe::PrepareInput {
                actual: &actual,
                context: &actual.context,
                catalog: &catalog,
                images: &pins.images,
                now_ms: chrono::Utc::now().timestamp_millis().try_into()?,
                base: source.base.clone(),
                retention: source.retention,
            })
            .context("gate04_recipe")?,
        );
        let key = format!("aq-finite-upstream-{}", Uuid::new_v4());
        let key_file = root.join("finite-peer-key");
        private(&key_file, key.as_bytes(), 0o600)?;
        Box::pin(install(&mut actual, &recipe, &key_file))
            .await
            .context("gate04_install")?;
        let evidence = Box::pin(execute_case(
            &root,
            &pins,
            &mut actual,
            recipe,
            &source,
            &graph,
            &key_file,
            &key,
        ))
        .await
        .context("gate04_execute")?;
        private(
            &root.join("qualified-evidence.json"),
            &serde_json::to_vec_pretty(&evidence)?,
            0o600,
        )?;
        Ok::<_, anyhow::Error>(
            json!({"evidence":evidence,"initial_host_free_bytes":free,"private_audit_root":root}),
        )
    })
    .await;
    source.owner.stop().await;
    let graph_cleanup = graph.shutdown().await.context("gate04_graph_cleanup")?;
    ensure!(
        graph_cleanup.child_reaped
            && graph_cleanup.database_removed
            && graph_cleanup.directory_removed,
        "actual Graph cleanup incomplete"
    );
    result
}
fn refusal_stage(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        match cause.to_string().as_str() {
            "gate04_catalog" => return "catalog",
            "gate04_project" => return "project_definition",
            "gate04_actual_setup" => return "actual_project_setup",
            "gate04_recipe" => return "runtime_recipe",
            "gate04_install" => return "owner_install",
            "gate04_execute" => {}
            "gate04_worker_wait" | "ordinary RunNative failed" => return "worker_exit",
            "actual physical stop/paired ACK required" => return "physical_ack_assertion",
            "actual private auth/fixture sender cleanup" => return "private_cleanup",
            "gate04_tool_validation" => return "tool_validation",
            "gate04_model_input" => return "model_input",
            "gate04_terminal_tail" => return "terminal_tail",
            "actual fixed file differs" => return "fixed_file",
            "gate04_graph_cleanup" => return "graph_cleanup",
            _ => {}
        }
    }
    if error.chain().any(|e| e.to_string() == "gate04_execute") {
        "ordinary_worker_execution"
    } else {
        "guard_or_setup_unavailable"
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit pinned worker0.5.1/Codex/tmux/aq-git/Graph, disposable PostgreSQL and fresh SID; finite local provider only"]
async fn actual_finite_codex_task_claim_source_and_protected_graph_closure() {
    let result = Box::pin(actual_case()).await;
    if let Err(error) = &result {
        eprintln!(
            "GATE04_OUTCOME_REFUSAL stage={} no_success_claim",
            refusal_stage(error)
        );
    }
    assert!(
        result.is_ok(),
        "actual finite Codex composed qualification failed"
    );
    eprintln!("GATE04_QUALIFIED {}", result.unwrap());
}
