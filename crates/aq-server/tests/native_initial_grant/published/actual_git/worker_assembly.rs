//! Independent pinned worker process + actual daemon HTTP/SQL + actual Git.
//! Runtime/harness and inventory qualification remains an explicit fixture.
use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::io::AsyncReadExt;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned native worker smoke binary and actual aq-git/Git"]
async fn actual_worker_downloads_installs_hydrates_and_prepares_against_real_daemon() {
    let mut a = setup_actual_unpublished().await;
    let case = &mut a.case;
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
    let http_statuses = Arc::new(std::sync::Mutex::new(Vec::<(String, u16, u128)>::new()));
    let observed = http_statuses.clone();
    let app = aq_server::build_worker_router(case.env.env.state.clone()).layer(
        axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let observed = observed.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let started = std::time::Instant::now();
                    let response = next.run(request).await;
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
        ),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let executable = std::env::var("AQ_TEST_NATIVE_WORKER").expect("explicit pinned smoke binary");
    let expected =
        std::env::var("AQ_TEST_NATIVE_WORKER_SHA256").expect("explicit smoke binary digest");
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
    let request = json!({"worker":{"server_url":format!("http://{address}"),"node_id":case.env.env.node,"credential":case.env.env.credential},"root":worker_root,"git_socket":worker_socket,"git_root":worker_git_root,"runtime":qualified.runtime,"qualification":qualified.qualification,"capabilities":qualified.capabilities,"features":qualified.features,"renderer":config.context_qualifications[0].artifact().unwrap()});
    input
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let ready = read_frame(&mut output).await;
    assert_eq!(ready["phase"], "ready");
    let identity: aq_admission::transport::WorkerIdentity =
        serde_json::from_value(ready["identity"].clone()).unwrap();
    assert_eq!(identity.node_id, case.env.env.node);
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
    input.write_all(b"{\"continue\":true}\n").await.unwrap();
    let result = read_frame(&mut output).await;
    if result["phase"] != "granted" {
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
    assert_eq!(
        result["phase"],
        "granted",
        "{result}; HTTP statuses: {:?}",
        http_statuses.lock().unwrap()
    );
    let prepare: Prepare = serde_json::from_value(result["request"].clone()).unwrap();
    let grant: InitialGrantDelivery = serde_json::from_value(result["grant"].clone()).unwrap();
    assert_eq!(prepare.identity, identity);
    assert_eq!(grant.original.grant.binding, prepare.binding);
    assert!(grant.original.grant.expires_at_ms <= grant.execution_deadline_ms);
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
}
