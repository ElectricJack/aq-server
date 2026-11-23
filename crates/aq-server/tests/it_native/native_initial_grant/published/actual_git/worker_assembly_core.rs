// Shared actual worker/Git/daemon physical test assembly; no profile conversion.
use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncReadExt;
type HttpStatuses = Arc<std::sync::Mutex<Vec<(String, u16, u128)>>>;
async fn read_frame(reader: &mut BufReader<tokio::process::ChildStdout>) -> Value {
    let mut line = String::new();
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        (&mut *reader).take(262145).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        n > 0 && n <= 262144,
        "bounded worker evidence frame missing"
    );
    serde_json::from_str(&line).unwrap()
}
#[derive(Clone, Copy)]
#[allow(dead_code)] // The refresh fixture deliberately selects only Output.
enum Mode {
    Legacy,
    Accepted,
    Inputs,
    Execution,
    Host,
    LostExecutionReply,
    Terminal,
    Output,
}
impl Mode {
    fn uses_phase(self) -> bool {
        !matches!(self, Mode::Legacy)
    }

    fn prepares_inputs(self) -> bool {
        matches!(
            self,
            Mode::Inputs
                | Mode::Execution
                | Mode::Host
                | Mode::LostExecutionReply
                | Mode::Terminal
                | Mode::Output
        )
    }

    fn executes_phase(self) -> bool {
        matches!(
            self,
            Mode::Execution | Mode::Host | Mode::LostExecutionReply | Mode::Terminal | Mode::Output
        )
    }

    fn starts_host(self) -> bool {
        matches!(self, Mode::Host | Mode::Terminal | Mode::Output)
    }

    fn reports_terminal(self) -> bool {
        matches!(self, Mode::Terminal | Mode::Output)
    }

    fn relays_output(self) -> bool {
        matches!(self, Mode::Output)
    }

    fn loses_execution_reply(self) -> bool {
        matches!(self, Mode::LostExecutionReply)
    }
}

async fn configure_phase(case: &mut Case, mode: Mode) -> (Option<Value>, Option<ArtifactRef>) {
    if !mode.uses_phase() {
        return (None, None);
    }
    let (mut config, inventory_qualification) =
        phase::configure(case, mode.executes_phase()).await;
    config["prepare_inputs"] = json!(mode.prepares_inputs());
    if mode.executes_phase() {
        config["prepare_execution"] = json!(true);
    }
    if mode.loses_execution_reply() {
        config["history_after_lost_execution"] = json!(true);
    }
    if mode.starts_host() {
        config["synthetic_host"] = phase_host::configuration();
    }
    if mode.reports_terminal() {
        phase_terminal::configure(case).await;
        config["report_terminal"] = json!(true);
    }
    if mode.relays_output() {
        phase_output::configure(case).await;
        config["relay_output"] = json!(true);
        config["await_synthetic_assessment"] = json!(true);
    }
    (Some(config), inventory_qualification)
}

async fn verify_legacy_grant(
    case: &Case,
    result: &Value,
    identity: &aq_admission::transport::WorkerIdentity,
    worker_root: &std::path::Path,
    worker_git_root: &std::path::Path,
) {
    let prepare: Prepare = serde_json::from_value(result["request"].clone()).unwrap();
    let grant: InitialGrantDelivery = serde_json::from_value(result["grant"].clone()).unwrap();
    assert_eq!(&prepare.identity, identity);
    assert_eq!(grant.original.grant.binding, prepare.binding);
    assert!(grant.original.grant.expires_at_ms <= grant.execution_deadline_ms);
    // Inspect actual disk history only after the independent worker's live
    // owner reports success. Reading this record never grants execution rights.
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(worker_root.join("journal/journal.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot["state"]["format"], JOURNAL_FORMAT);
    let attempt = &snapshot["state"]["attempts"][prepare.binding.attempt_id.to_string()];
    let record = &attempt["initial_grant"];
    assert_eq!(record["delivery"], result["grant"]);
    assert_eq!(attempt["prepared"]["request"], result["request"]);
    assert_eq!(
        record["prepare_digest"],
        attempt["prepared"]["request_digest"]
    );
    let mut canonical = record.clone();
    canonical.sort_all_objects();
    assert_eq!(
        result["durable_grant"]["record_digest"],
        json!(Digest::of(&serde_json::to_vec(&canonical).unwrap()))
    );
    assert_eq!(
        result["durable_grant"]["attempt_id"],
        json!(prepare.binding.attempt_id)
    );
    assert_eq!(record["record_revision"], snapshot["state"]["revision"]);
    assert!(record["record_revision"].as_u64().unwrap() >= prepare.journal_revision + 2);
    assert!(
        result["durable_grant"]["observed_after_sync_ns"]
            .as_u64()
            .unwrap()
            >= record["observed_before_write_ns"].as_u64().unwrap()
    );
    for cap in ["deadline_ns", "hard_deadline_ns"] {
        assert!(result["durable_grant"][cap].as_u64().unwrap() <= record[cap].as_u64().unwrap());
        assert!(
            result["durable_grant"]["observed_after_sync_ns"]
                .as_u64()
                .unwrap()
                < result["durable_grant"][cap].as_u64().unwrap()
        );
    }
    let row: Value =
        sqlx::query_scalar("SELECT receipt FROM admission_material_receipts WHERE lease_id=$1")
            .bind(prepare.materials[0].lease_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(row, serde_json::to_value(&prepare.materials[0]).unwrap());
    let path = std::path::PathBuf::from(result["workspace_path"].as_str().unwrap());
    assert!(path.starts_with(worker_git_root.join("workspaces")));
    assert_eq!(
        std::fs::read(path.join("source")).unwrap(),
        b"actual source bytes\n"
    );
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(prepare.binding.attempt_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(hot["status"], "granted");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='started'"
        )
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap(),
        0
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(prepare.binding.attempt_id)
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap()
    );
}

async fn diagnose_worker_history(
    case: &Case,
    worker_root: &std::path::Path,
    identity: &aq_admission::transport::WorkerIdentity,
) {
    // A rejected delivery does not imply that the server issued no grant.
    // Retain only bounded historical metadata, never material or credentials.
    let issued: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('operation_id',r.operation_id,'request_digest',r.request_digest,'grant_id',r.response#>'{body,grant,grant_id}','binding',r.response#>'{body,grant,binding}','versions',r.response->'versions','original_clock',r.response#>'{body,clock}','grant_expiry',r.response#>'{body,grant,expires_at_ms}','execution_deadline_ms',r.response->'execution_deadline_ms','holds_task',a.holds_task,'hot_status',h.state->'status') FROM profiled_execution_receipts r JOIN execution_attempts a ON a.attempt_id::text=r.response#>>'{body,grant,binding,attempt_id}' JOIN profiled_authority_hot h ON h.attempt_id=a.attempt_id WHERE r.node_id=$1 AND r.kind='prepare' AND r.response->>'kind'='grant' ORDER BY r.operation_id LIMIT 2")
        .bind(identity.node_id).fetch_all(&case.env.env.pool).await.unwrap();
    eprintln!(
        "bounded central issued-grant history (not live worker permission): {}",
        json!(issued)
    );
    let clocks: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('now',decision#>'{inputs,now_ms}','budget_expiry',decision#>'{inputs,budget,expires_at_ms}','selection_expiry',decision#>'{outcome,resolved,expires_at_ms}') FROM intelligence_decisions ORDER BY decision#>>'{inputs,now_ms}' DESC LIMIT 5").fetch_all(&case.env.env.pool).await.unwrap();
    eprintln!("bounded original decision clocks: {}", json!(clocks));
    let snapshot: Value = serde_json::from_slice(
        &std::fs::read(worker_root.join("journal/journal.json")).unwrap(),
    )
    .unwrap();
    let attempts = snapshot["state"]["attempts"].as_object().unwrap();
    let a = attempts.values().next().cloned().unwrap_or(Value::Null);
    let downloads = a["downloads"].as_object().cloned().unwrap_or_default();
    let completed = downloads
        .values()
        .filter(|d| !d["receipt"].is_null())
        .count();
    eprintln!(
        "bounded worker timing: {}",
        json!({"server_now_ms":chrono::Utc::now().timestamp_millis(),"preparation_clock":a["preparation"]["delivery"]["original"]["clock"],"sent_ns":a["preparation"]["sent_ns"],"received_ns":a["preparation"]["received_ns"],"deadline_ns":a["preparation"]["deadline_ns"],"download_count":downloads.len(),"completed_count":completed,"last_completion_ns":downloads.values().filter_map(|d|d["completed_ns"].as_u64()).max()})
    );
}

async fn run_worker(mode: Mode) {
    let use_phase = mode.uses_phase();
    let execute_phase = mode.executes_phase();
    let host = mode.starts_host();
    let terminal = mode.reports_terminal();
    let relay_output = mode.relays_output();
    let history_loss = mode.loses_execution_reply();
    let mut a = setup_actual_unpublished().await;
    let case = &mut a.case;
    if REFRESH_PROFILE {
        refresh_identity::configure(case).await;
    }
    let (phase_config, inventory_qualification) = configure_phase(case, mode).await;
    let dir = case.env.env._dir.path();
    let worker_root = dir.join("assembly-worker");
    std::fs::create_dir(&worker_root).unwrap();
    std::fs::set_permissions(&worker_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let worker_git_root = worker_root.join("git");
    let worker_socket = worker_root.join("git.sock");
    let mut git_child =
        tokio::process::Command::new(std::env::var("AQ_TEST_GIT_CANDIDATE").unwrap())
            .args([
                "--state-dir",
                worker_git_root.to_str().unwrap(),
                "--git-executable",
                &std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap(),
                "--listen",
                worker_socket.to_str().unwrap(),
                "serve",
            ])
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
    for _ in 0..200 {
        if worker_socket.exists() {
            break;
        }
        assert!(git_child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(worker_socket.exists());
    let repo = case.env.frozen.project.binding.repository_id;
    rpc(
        &worker_socket,
        repo,
        "repo.register",
        json!({"identity":"native-real-git-fixture","mode":"worker","binding_generation":1}),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http_statuses = HttpStatuses::default();
    let observed = http_statuses.clone();
    let output_exchanges = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_output = output_exchanges.clone();
    let admission_clock = case.env.env.state.clock.clone();
    let mut app = aq_server::build_worker_router(case.env.env.state.clone());
    if execute_phase && !terminal {
        let routes = phase::execution_routes(case.env.env.state.clone());
        app = app.merge(if history_loss {
            phase_history::withhold_first_success(routes)
        } else {
            routes
        });
    }
    let app = app.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let observed = observed.clone();
            let captured_output = captured_output.clone();
            let admission_clock = admission_clock.clone();
            async move {
                let path = request.uri().path().to_owned();
                let started = std::time::Instant::now();
                let request_started_ms = (admission_clock)().timestamp_millis();
                let (request, output_upload) = phase_output::capture_upload_request(request).await;
                let response = next.run(request).await;
                if matches!(
                    path.as_str(),
                    "/v1/worker/profiled-v2/accept" | "/v1/worker/profiled-v2/prepare"
                ) {
                    let classification =
                        response
                            .extensions()
                            .get::<aq_server::test_support::control_diagnostics::ConflictClass>();
                    eprintln!(
                        "bounded {} response: {}",
                        if path == "/v1/worker/profiled-v2/accept" {
                            "accept"
                        } else {
                            "prepare"
                        },
                        json!({
                            "request_started_at_ms":request_started_ms,
                            "response_completed_at_ms":(admission_clock)().timestamp_millis(),
                            "status":response.status().as_u16(),
                            "conflict_class":classification
                        })
                    );
                }
                phase_output::record_upload(&captured_output, output_upload, response.status());
                let mut values = observed.lock().unwrap();
                if values.len() < 160 {
                    values.push((
                        path,
                        response.status().as_u16(),
                        started.elapsed().as_millis(),
                    ));
                }
                response
            }
        },
    ));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let executable = std::env::var(WORKER_BINARY_ENV).expect("explicit pinned smoke binary");
    let expected = std::env::var(WORKER_DIGEST_ENV).expect("explicit smoke binary digest");
    assert_eq!(
        Digest::of(&std::fs::read(&executable).unwrap()).as_str(),
        expected
    );
    let mut worker = tokio::process::Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = worker.stdin.take().unwrap();
    let mut output = BufReader::new(worker.stdout.take().unwrap());
    let config = case
        .env
        .env
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap();
    let qualified = &config.qualified_runtimes[0];
    let mut request = json!({"worker":{"server_url":format!("http://{address}"),"node_id":case.env.env.node,"credential":case.env.env.credential},"root":worker_root,"git_socket":worker_socket,"git_root":worker_git_root,"runtime":qualified.runtime,"qualification":qualified.qualification,"capabilities":qualified.capabilities,"features":qualified.features,"renderer":aq_server::profiled_execution::context_budget::Renderer::rule().unwrap().definition});
    request[PHASE_CONFIG_FIELD] = json!(phase_config);
    input
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let ready = read_frame(&mut output).await;
    assert_eq!(ready["phase"], "ready");
    if REFRESH_PROFILE {
        assert_eq!(ready["worker_version"], "0.4.0");
        assert_eq!(ready["journal_format"], 11);
        assert_eq!(
            ready["preparation_contract"],
            json!(aq_admission::preparation_refresh_v1::contract_artifact().unwrap())
        );
    }
    let identity: aq_admission::transport::WorkerIdentity =
        serde_json::from_value(ready["identity"].clone()).unwrap();
    assert_eq!(identity.node_id, case.env.env.node);
    if execute_phase {
        phase::record_current_inventory(case, &identity, inventory_qualification.as_ref().unwrap())
            .await;
        phase::record_operator_availability(case).await;
    } else {
        // A new fixture observation for the real process's newly created boot.
        // No offer/selection exists yet and no sealed observation is retimed.
        let mut snapshot = case.env.input.node.clone();
        snapshot.target = aq_selection::ExecutionTarget::Worker {
            node_id: identity.node_id,
            boot_id: identity.boot_id,
        };
        let now = chrono::Utc::now().timestamp_millis() as u64;
        snapshot.revision += 1;
        snapshot.observed_at_ms = now;
        snapshot.expires_at_ms = now + 30000;
        for c in &mut snapshot.credential_bindings {
            c.observed_at_ms = now;
            c.expires_at_ms = now + 30000;
        }
        sqlx::query("UPDATE intelligence_node_snapshots SET boot_id=$2,sequence=1,revision=revision+1,snapshot=$3 WHERE node_id=$1")
            .bind(identity.node_id).bind(identity.boot_id).bind(json!(snapshot)).execute(&case.env.env.pool).await.unwrap();
    }
    input.write_all(b"{\"continue\":true}\n").await.unwrap();
    let mut result = read_frame(&mut output).await;
    if relay_output && result["phase"] == "prepared_inputs_awaiting_synthetic_assessment" {
        phase_assessment::assess(
            case,
            &worker_root,
            &identity,
            &result,
            &request[PHASE_CONFIG_FIELD]["synthetic_host"],
        )
        .await;
        input.write_all(b"{\"continue\":true}\n").await.unwrap();
        result = read_frame(&mut output).await;
    }
    let watch = if relay_output {
        if result["phase"] != "terminal_registered" {
            phase::diagnostics(case).await;
            phase_output::refusal_diagnostics(&worker_root, &result, &http_statuses);
        }
        let watched = phase_output::watch(case, &result).await;
        input.write_all(b"{\"continue\":true}\n").await.unwrap();
        result = read_frame(&mut output).await;
        Some(watched)
    } else {
        None
    };
    let expected_phase = if history_loss {
        "phase_history_only"
    } else if execute_phase {
        "granted_phase_execution"
    } else if mode.prepares_inputs() {
        "prepared_phase_inputs"
    } else if use_phase {
        "accepted_preparation"
    } else {
        "granted"
    };
    if result["phase"] != expected_phase {
        if execute_phase {
            phase::diagnostics(case).await;
        }
        diagnose_worker_history(case, &worker_root, &identity).await;
    }
    assert_eq!(
        result["phase"],
        expected_phase,
        "{result}; HTTP statuses: {:?}",
        http_statuses.lock().unwrap()
    );
    if history_loss {
        let statuses = http_statuses.lock().unwrap();
        for (path, expected) in [
            ("/v1/worker/profiled-v2/prepare", 503),
            ("/v1/worker/profiled-v2/history", 200),
        ] {
            let seen: Vec<_> = statuses
                .iter()
                .filter(|(p, _, _)| p == path)
                .map(|(_, status, _)| *status)
                .collect();
            assert_eq!(seen, vec![expected], "exactly one request to {path}");
        }
    }
    if use_phase {
        phase::verify(case, &worker_root, &identity, &result).await;
        if host {
            phase_host::verify(&worker_root, &result);
            if terminal {
                phase_terminal::verify(case, &worker_root, &result).await;
                if let Some(watched) = &watch {
                    phase_output::verify(case, watched, &result, &output_exchanges).await;
                }
            }
        } else {
            assert!(result.get("host_started").is_none());
        }
    } else {
        verify_legacy_grant(case, &result, &identity, &worker_root, &worker_git_root).await;
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), worker.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.abort();
    git_child.kill().await.unwrap();
    git_child.wait().await.unwrap();
    a.process.kill().await.unwrap();
    a.process.wait().await.unwrap();
    if terminal && std::env::var("AQ_TEST_RETAIN_NATIVE_DEMO").as_deref() == Ok("1") {
        // Explicit local preview only, after all assertions and physical cleanup.
        // Retain the real database rather than fabricate dashboard receipt rows.
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
        std::mem::forget(case.env.env._db.clone());
        eprintln!("retained_native_demo_database={name}");
    }
}
