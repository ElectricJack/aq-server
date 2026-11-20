use super::*;
use crate::playbook_host9::GraphMergeHost;
use aq_playbook_host::model::schedules::BindingSchedule;

const INTERVAL: u64 = 30_000;

fn psql(client: &Path, service_file: &Path) -> Command {
    let mut command = Command::new(client);
    command
        .env("PGSERVICEFILE", service_file)
        .env("PGSERVICE", "aq_graph_deploy")
        .args(["-XAtw", "-v", "ON_ERROR_STOP=1"]);
    command
}

fn private(path: &Path, content: String) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

async fn read_scope(
    canary: &Canary,
    source: &GraphMergeHost,
    merge: Uuid,
    other_revision: &str,
    client: &Path,
) -> PathBuf {
    let host = source.fixture();
    let pool = host.db.pool().await;
    let project = host.selection.lineage.project_id;
    let enrollment = (host.state.clock)().timestamp_millis();

    // The current origin/main commit is already built. Even so, rows from a
    // different project, repository, branch or enrollment window are invisible.
    for (project_id, repo, target_ref, observed) in [
        (
            Uuid::new_v4(),
            source.repository,
            "refs/heads/main",
            enrollment,
        ),
        (project, Uuid::new_v4(), "refs/heads/main", enrollment),
        (project, source.repository, "refs/heads/feature", enrollment),
        (
            project,
            source.repository,
            "refs/heads/main",
            enrollment - 1,
        ),
    ] {
        sqlx::query(
            "INSERT INTO delivery_merge_evidence
                 (operation_id, project_id, task_id, pull_request, target, target_ref,
                  expected_head_sha, merge_sha, observed_at_ms)
             SELECT $1, $2, task_id, pull_request,
                    jsonb_set(target, '{git,repository_id}', to_jsonb($3::uuid)),
                    $4, expected_head_sha, $5, $6
             FROM delivery_merge_evidence WHERE operation_id=$7",
        )
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(repo)
        .bind(target_ref)
        .bind(other_revision)
        .bind(observed)
        .bind(merge)
        .execute(&pool)
        .await
        .unwrap();
    }
    let role = format!("aq_graph_reader_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    sqlx::query(&format!(
        "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD '{password}'"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let options = host.db.options();
    let database = options.get_database().unwrap();
    let base = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    let service = |user: &str, password: &str| {
        format!(
            "[aq_graph_deploy]\nhost={}\nport={}\ndbname={database}\nuser={user}\npassword={password}\n",
            base.host_str().unwrap(),
            base.port().unwrap(),
        )
    };
    let admin = canary.directory.join("source-admin.conf");
    private(&admin, service(base.username(), base.password().unwrap()));
    command(
        psql(client, &admin)
            .arg("-f")
            .arg(proof_root().join("host10-read-scope.sql"))
            .args([
                "-v",
                &format!("reader={role}"),
                "-v",
                &format!("project_id={project}"),
                "-v",
                &format!("repository_id={}", source.repository),
                "-v",
                "binding_generation=1",
                "-v",
                &format!("enrolled_after_ms={enrollment}"),
            ]),
    );
    fs::remove_file(admin).unwrap();

    let reader = canary.directory.join("source-reader.conf");
    private(&reader, service(&role, &password));
    assert_eq!(
        command(psql(client, &reader).args([
            "-c",
            "SELECT operation_id FROM aq_graph_deploy_export.merged",
        ])),
        merge.to_string()
    );
    let denied = psql(client, &reader)
        .args(["-c", "SELECT * FROM public.delivery_merge_evidence"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("permission denied"));
    let denied = psql(client, &reader)
        .args([
            "-c",
            "DELETE FROM aq_graph_deploy_export.merged WHERE false",
        ])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("permission denied"));
    reader
}

async fn schedule(canary: &Canary) {
    canary
        .driver
        .ensure_binding(
            BINDING.into(),
            canary.artifact.clone(),
            canary.grant,
            &canary.scope,
            &[BindingSchedule {
                key: "graph-merge".into(),
                every_ms: INTERVAL,
            }],
        )
        .await
        .unwrap();
}

async fn next_occurrence(canary: &Canary) -> String {
    let inspect = || {
        canary
            .driver
            .inspect_runs(canary.f.actor, Default::default())
    };
    let previous: BTreeSet<_> = inspect()
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|run| run.id)
        .collect();
    canary.f.time.fetch_add(INTERVAL as i64, Ordering::SeqCst);
    for _ in 0..24 {
        canary.driver.quantum().await.unwrap();
        if let Some(run) = inspect()
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|run| run.entry == "automatic" && !previous.contains(&run.id))
        {
            return run.id;
        }
    }
    panic!("Host11 schedule did not admit an automatic run");
}

async fn deployment(
    canary: &Canary,
    run: &str,
    merge: Uuid,
    revision: &str,
    generation: u64,
) -> String {
    let registered = canary.wait_for_receipt(run, "registered").await;
    assert_eq!(registered["merge_operation_id"], json!(merge));
    assert_eq!(registered["generation"], generation);
    tick(&canary.plugin, &canary.directory);
    let provider = canary.provider();
    canary.ready().await;
    canary.healthy(run, revision, generation, &provider).await;
    println!("Host10 merge operation {merge} registered generation {generation}");
    provider
}

#[tokio::test]
#[ignore = "requires scripts/test-graph-deployment.py --automatic and an isolated PostgreSQL reader"]
async fn host10_evidence_drives_host11() {
    let graph_repo = PathBuf::from(std::env::var_os("AQ_GRAPH_DEPLOY_GRAPH_REPO").unwrap());
    let client = PathBuf::from(std::env::var_os("AQ_GRAPH_DEPLOY_PSQL").unwrap());
    let revisions = std::env::var("AQ_GRAPH_DEPLOY_REVISIONS").unwrap();
    let (older, newer) = revisions.split_once(',').unwrap();

    let source = GraphMergeHost::new().await;
    let first = source.merge(older).await;
    let mut canary = prepare().await;
    assert_ne!(
        source.fixture().db.options().get_database(),
        canary.f.db.options().get_database()
    );
    canary.opt_in("");
    schedule(&canary).await;
    let disabled = next_occurrence(&canary).await;
    canary.finish(&disabled, "refused").await;
    assert_eq!(
        canary.receipt(&disabled, "refused").await.unwrap()["reason"],
        "automatic_deployment_disabled"
    );

    let reader = read_scope(&canary, &source, first, newer, &client).await;
    canary.opt_in(&format!(
        "merge_source = {reader:?}\nmerge_client = {client:?}\n"
    ));
    let waiting = next_occurrence(&canary).await;
    canary.finish(&waiting, "idle").await;
    assert_eq!(
        canary.receipt(&waiting, "idle").await.unwrap()["reason"],
        "waiting_for_build"
    );
    assert!(canary.receipt(&waiting, "registered").await.is_none());
    assert!(
        !canary
            .directory
            .join(format!("provenance/{older}.json"))
            .exists()
    );

    let builder = command(
        Command::new("python3")
            .arg(proof_root().join("build-image.py"))
            .arg(&canary.directory)
            .arg(&graph_repo),
    );
    assert!(builder.contains(&format!("Building aq-graph {older}")));
    println!("{builder}");
    let first_run = next_occurrence(&canary).await;
    let first_provider = deployment(&canary, &first_run, first, older, 1).await;

    // A new owner sees the same Host10 rows and next interval, with no new
    // generation and no replacement of the already healthy container.
    canary.driver.stop().await.unwrap();
    canary.driver = start(&canary.service, &canary.f).await;
    canary.driver.recover().await.unwrap();
    schedule(&canary).await;
    let repeat = next_occurrence(&canary).await;
    canary.finish(&repeat, "idle").await;
    assert_eq!(
        canary.receipt(&repeat, "idle").await.unwrap()["reason"],
        "no_merge"
    );
    assert_eq!(canary.provider(), first_provider);

    let second = source.merge(newer).await;
    assert_ne!(first, second);
    let second_run = next_occurrence(&canary).await;
    let second_provider = deployment(&canary, &second_run, second, newer, 2).await;
    assert_ne!(first_provider, second_provider);

    // A second Host10 operation for the older revision is observed after the
    // newer instance is healthy. It cannot allocate a new generation.
    source.merge(older).await;
    let late = next_occurrence(&canary).await;
    canary.finish(&late, "idle").await;
    assert_eq!(
        canary.receipt(&late, "idle").await.unwrap()["reason"],
        "no_merge"
    );
    tick(&canary.plugin, &canary.directory);
    assert_eq!(canary.provider(), second_provider);
    canary.driver.stop().await.unwrap();
}
