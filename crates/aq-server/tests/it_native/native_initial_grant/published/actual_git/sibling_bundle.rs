use super::*;

#[tokio::test]
#[ignore = "qualification requires explicit actual aq-git and Git executable paths"]
async fn sibling_bundle_http_hydrates_hidden_commit_and_refuses_revoked_source() {
    let mut actual = setup_actual().await;
    let c = &mut actual.prepared;
    let env = &mut c.case.env;
    let pool = &env.env.pool;
    let attempt = c.request.binding.attempt_id;
    let home = env.lineage.project_id;
    let task = env.lineage.task_id;
    let (project, repository) = (Uuid::new_v4(), Uuid::new_v4());
    let root = env.env._dir.path();
    let remote = root.join("sibling-seed");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-b", "main"]).await;
    git(&remote, &["commit", "--allow-empty", "-m", "advertised"]).await;
    git(&remote, &["switch", "--orphan", "approved"]).await;
    std::fs::write(remote.join("proof"), b"retained sibling source\n").unwrap();
    git(&remote, &["add", "proof"]).await;
    git(&remote, &["commit", "-m", "hidden source"]).await;
    let base = git(&remote, &["rev-parse", "HEAD"]).await;
    git(&remote, &["switch", "main"]).await;
    rpc(
        &actual.socket,
        repository,
        "repo.register",
        json!({
            "identity":"hidden-sibling-source","mode":"central","remote_url":remote,
            "allow_file_transport":true,"allowed_refs":["refs/heads/main","refs/heads/approved"],
            "allowed_targets":["refs/heads/main"]
        }),
    )
    .await;
    let retained = rpc(
        &actual.socket,
        repository,
        "repo.retain_ref",
        json!({"ref":"refs/heads/approved"}),
    )
    .await;
    assert_eq!(retained["head"]["hex"], base);
    let source_handle: Uuid = serde_json::from_value(retained["retention_handle"].clone()).unwrap();
    git(&remote, &["branch", "-D", "approved"]).await;
    let heads = git(
        &remote,
        &["for-each-ref", "--format=%(objectname)", "refs/heads"],
    )
    .await;
    assert!(!heads.contains(&base));
    let central_pool = root
        .join("actual-git/repositories")
        .join(repository.to_string())
        .join("objects.git");
    let advertised = git(
        &central_pool,
        &["for-each-ref", "--format=%(objectname)", "refs/heads"],
    )
    .await;
    assert!(!advertised.contains(&base));

    Arc::make_mut(&mut env.env.state.config).delivery = Some(aq_delivery::github::DeliveryConfig {
        git_http_socket: Some(actual.socket.with_extension("http")),
        ..Default::default()
    });
    sqlx::query(
        "INSERT INTO project_execution_bindings
       (project_id,revision,binding,validated_at_ms,binding_observation)
       VALUES($1,1,jsonb_build_object('enabled',true,'repository_id',$2::text,
                                      'repository_binding_generation',1),1,'{}')",
    )
    .bind(project)
    .bind(repository.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_workspace_sources
      (home_project_id,task_id,target_project_id,project_revision,repository_id,
       binding_generation,commit_oid,access,state)
      VALUES($1,$2,$3,1,$4,1,$5,'read','approved')",
    )
    .bind(home)
    .bind(task)
    .bind(project)
    .bind(repository)
    .bind(&base)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_workspace_retained_bases
      (repository_id,binding_generation,commit_oid,source_handle) VALUES($1,1,$2,$3)",
    )
    .bind(repository)
    .bind(&base)
    .bind(source_handle)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO task_workspace_offer_fences(attempt_id,scope)
        SELECT $1,jsonb_build_object('sources',jsonb_build_array(to_jsonb(s)),'services','[]'::jsonb)
        FROM task_workspace_sources s WHERE s.home_project_id=$2 AND s.task_id=$3 AND s.target_project_id=$4
        ON CONFLICT(attempt_id) DO NOTHING")
        .bind(attempt).bind(home).bind(task).bind(project).execute(pool).await.unwrap();
    let app = aq_server::build_worker_router(env.env.state.clone());
    let get = |path: String, auth: bool| {
        let mut request = axum::http::Request::get(path).header("aq-attempt", attempt.to_string());
        if auth {
            request = request.header("authorization", format!("Bearer {}", env.env.credential));
        }
        app.clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
    };
    let grants = get(format!("/v1/git/grants/{attempt}/workspaces"), true)
        .await
        .unwrap();
    let status = grants.status();
    let bytes = grants.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["workspaces"][0]["base"], base);
    let path = format!("/v1/git/workspaces/{project}/{repository}/aq-source-bundle?offset=0");
    assert_eq!(get(path.clone(), false).await.unwrap().status(), 401);
    let reply = get(path.clone(), true).await.unwrap();
    let status = reply.status();
    let headers = reply.headers().clone();
    let bytes = reply.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(headers["content-type"], "application/x-git-bundle");
    assert_eq!(headers["aq-workspace-bundle-size"], bytes.len().to_string());
    assert_eq!(
        headers["aq-workspace-bundle-sha256"],
        aq_admission::Digest::of(&bytes).as_str()
    );
    let repeat = get(path.clone(), true).await.unwrap();
    assert_eq!(repeat.status(), 200);
    assert_eq!(
        repeat.into_body().collect().await.unwrap().to_bytes(),
        bytes
    );
    let bundle = root.join("authenticated.bundle");
    std::fs::write(&bundle, bytes).unwrap();
    let fresh = root.join("fresh-sibling");
    std::fs::create_dir(&fresh).unwrap();
    git(&fresh, &["init"]).await;
    git(&fresh, &["bundle", "unbundle", bundle.to_str().unwrap()]).await;
    git(&fresh, &["checkout", "--detach", &base]).await;
    assert_eq!(
        std::fs::read(fresh.join("proof")).unwrap(),
        b"retained sibling source\n"
    );
    sqlx::query("UPDATE task_workspace_sources SET state='denied' WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3")
        .bind(home).bind(task).bind(project).execute(pool).await.unwrap();
    assert_eq!(get(path, true).await.unwrap().status(), 403);
}
