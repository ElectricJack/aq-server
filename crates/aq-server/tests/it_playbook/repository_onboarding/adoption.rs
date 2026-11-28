use super::*;

fn check_adopted(v: &Value) {
    assert_eq!(
        (
            v["disposition"].as_str(),
            v["default_branch"].as_str(),
            v["default_ref"].as_str()
        ),
        (Some("bound"), Some("trunk"), Some("refs/heads/trunk")),
        "{v}"
    );
    assert_eq!(
        (
            v["adopted"].as_bool(),
            v["repository_identity"].as_str(),
            v["repository_binding_generation"].as_u64()
        ),
        (Some(true), Some("historical-seed"), Some(1))
    );
    assert!(v["base"].is_null());
    assert!(v["repository_observation_id"].as_str().is_some());
}
async fn check_replay(o: &Onboarding, op: Uuid, seeded: &Value, v: &Value) {
    // Replay: the same answer, no second row, no registration, same project.
    assert_eq!(adopt(o, adopt_request(op)).await.1, *v);
    assert_eq!(rows(o).await, 1);
    {
        let f = o.fake.lock().unwrap();
        assert!(f.registers.is_empty() && f.syncs.is_empty());
    }
    let (status, project) = call(&o.app, &path(), None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(project, seeded["record"]);
}
async fn refuse_other_owner(o: &Onboarding, op: Uuid, owner2: &str) {
    // The operation id is bound to its request.
    let mut other = adopt_request(op);
    other["github"]["name"] = "other".into();
    other["remote_url"] = "https://github.com/acme/other.git".into();
    assert_eq!(adopt(o, other).await.0, StatusCode::CONFLICT);
    assert_eq!(
        call(
            &o.app,
            "/api/repositories/adopt",
            Some(adopt_request(op)),
            owner2
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Every successful reply belongs to the caller's exact operation.
    assert_eq!(
        adopt(o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(o.fake.lock().unwrap().metadata_auth, vec![None]);
}
fn mismatch_cases(remote: &str) -> Vec<(&str, &str, Value)> {
    let body = |f: &dyn Fn(&mut Value)| {
        let mut b = adopt_request(Uuid::new_v4());
        f(&mut b);
        b
    };
    vec![
        ("https://github.com/acme/other.git", "origin", body(&|_| {})),
        (
            "https://github.com/acme/widgets",
            "same repository but different origin spelling",
            body(&|_| {}),
        ),
        (
            remote,
            "invalid URL",
            body(&|b| b["remote_url"] = "https://user@github.com/acme/widgets.git".into()),
        ),
        ("", "no origin reported", body(&|_| {})),
        (
            remote,
            "expected generation",
            body(&|b| b["expected_binding_generation"] = 2.into()),
        ),
        (
            remote,
            "other project",
            body(&|b| b["project_id"] = json!(Uuid::from_u128(9))),
        ),
        (
            remote,
            "other repository",
            body(&|b| b["repository_id"] = json!(Uuid::from_u128(9))),
        ),
        (
            remote,
            "default branch",
            body(&|b| b["default_branch"] = "main".into()),
        ),
        (
            remote,
            "other github repo",
            body(&|b| b["github"]["owner"] = "mallory".into()),
        ),
    ]
}

#[tokio::test]
async fn adopting_task_branch_delivery_needs_no_direct_push_to_the_default_branch() {
    let o = onboarding().await;
    bind(&o.env).await;
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.show_overrides.insert("allowed_targets".into(), json!([]));
        f.show_overrides
            .insert("allowed_target_prefixes".into(), json!(["refs/heads/aq/"]));
    }
    let (status, body) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["disposition"], "bound");
}

#[tokio::test]
async fn adopting_a_seeded_project_records_a_replayable_delivery_target() {
    let o = onboarding().await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    let seeded = bind(&o.env).await;
    assert!(delivery_target(&o, 1).await.is_none());
    let owner2 = "another-owner@localhost";
    o.env
        .state
        .store
        .invite_user(owner2, aq_server::store::Role::Owner, chrono::Utc::now())
        .await
        .unwrap();
    // The viewer is refused before either adapter is called.
    assert_eq!(
        call(
            &o.app,
            "/api/repositories/adopt",
            Some(adopt_request(Uuid::new_v4())),
            "viewer@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let op = Uuid::new_v4();
    let (status, v) = adopt(&o, adopt_request(op)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    check_adopted(&v);
    {
        let target = delivery_target(&o, 1).await.expect("target resolves");
        assert_eq!(
            (target.github.owner.as_str(), target.github.name.as_str()),
            ("acme", "widgets")
        );
        assert_eq!(target.default_branch, "trunk");
        assert_eq!(target.git.repository_id, Uuid::from_u128(2));
        // A rebinding since the adoption makes the recorded target stale.
        assert!(delivery_target(&o, 2).await.is_none());
    }
    check_replay(&o, op, &seeded, &v).await;
    refuse_other_owner(&o, op, owner2).await;
    let (status, read) = call(
        &o.app,
        &format!("/api/repositories/onboard/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!((status, read), (StatusCode::OK, v));
    assert_eq!(rows(&o).await, 1);
}

#[tokio::test]
async fn ordinary_onboarding_keeps_its_target_and_operation_identity() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (_, onboarded) = onboard(&o, request(&o, op)).await;
    assert_eq!(onboarded["disposition"], "bound", "{onboarded}");
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::CONFLICT);
    assert_eq!(rows(&o).await, 1);
    assert_eq!(
        delivery_target(&o, 1).await.unwrap().default_branch,
        "trunk"
    );
    assert!(delivery_target(&o, 2).await.is_none());
}

#[tokio::test]
async fn adoption_refuses_mismatches_and_records_nothing() {
    let o = onboarding().await;
    bind(&o.env).await;
    let remote = "https://github.com/acme/widgets.git";
    for (origin, why, b) in mismatch_cases(remote) {
        o.fake.lock().unwrap().remote = origin.into();
        let (status, v) = adopt(&o, b.clone()).await;
        // `mallory/widgets` differs from the origin; an unknown project is absent.
        let expected = match why {
            "other project" => StatusCode::NOT_FOUND,
            "other github repo" | "invalid URL" => StatusCode::BAD_REQUEST,
            _ => StatusCode::CONFLICT,
        };
        assert_eq!(status, expected, "{why}: {v}");
    }
    // aq-git moved to another generation after the project binding was written.
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = remote.into();
        f.generation = 2;
    }
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "stale aq-git generation: {v}");
    // GitHub requires signed commits on the default branch.
    {
        let mut f = o.fake.lock().unwrap();
        f.generation = 1;
        f.signing = true;
    }
    let (status, v) = adopt(&o, adopt_request(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::CONFLICT, "signing: {v}");
    assert_eq!(rows(&o).await, 0);
    assert!(delivery_target(&o, 1).await.is_none());
}

#[tokio::test]
async fn adoption_requires_matching_central_identity_and_an_allowed_existing_default_ref() {
    let o = onboarding().await;
    bind(&o.env).await;
    o.fake.lock().unwrap().remote = "https://github.com/acme/widgets.git".into();
    for (key, value) in [
        ("identity", json!("")),
        ("repository_id", json!(Uuid::new_v4())),
        ("mode", json!("worker")),
        ("object_format", json!("sha256")),
        ("allowed_refs", json!(["refs/heads/other"])),
        ("allowed_targets", json!(["refs/heads/other"])),
    ] {
        o.fake
            .lock()
            .unwrap()
            .show_overrides
            .insert(key.into(), value);
        let (status, answer) = adopt(&o, adopt_request(Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{key}: {answer}");
        o.fake.lock().unwrap().show_overrides.clear();
    }
    o.fake.lock().unwrap().absent_ref = true;
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    {
        let mut f = o.fake.lock().unwrap();
        f.absent_ref = false;
        f.metadata_override =
            Some(json!({"full_name":"different/widgets","default_branch":"trunk"}));
    }
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(rows(&o).await, 0);
}

#[tokio::test]
async fn adoption_fences_project_revision_and_refuses_stale_generations() {
    let o = onboarding().await;
    bind(&o.env).await;
    let arrived = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    {
        let mut f = o.fake.lock().unwrap();
        f.remote = "https://github.com/acme/widgets.git".into();
        f.rules_gate = Some((arrived.clone(), proceed.clone()));
    }
    let op = Uuid::new_v4();
    let change =
        async {
            tokio::time::timeout(std::time::Duration::from_secs(5), arrived.notified())
                .await
                .expect("adoption reached GitHub");
            let (status, answer) = call(
            &o.app, &path(),
            Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"binding":o.env.body})),
            "owner@localhost",
        ).await;
            proceed.notify_one();
            assert_eq!(status, StatusCode::OK, "{answer}");
        };
    let ((status, answer), ()) = tokio::join!(adopt(&o, adopt_request(op)), change);
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(rows(&o).await, 0);
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::OK);

    let mut next = o.env.body.clone();
    next["repository_binding_generation"] = 2.into();
    o.env
        .generation
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let (status, answer) = call(
        &o.env.app,
        &path(),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":next})),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(adopt(&o, adopt_request(op)).await.0, StatusCode::CONFLICT);
    assert!(delivery_target(&o, 2).await.is_none());

    o.fake.lock().unwrap().generation = 2;
    let mut new = adopt_request(Uuid::new_v4());
    new["expected_binding_generation"] = 2.into();
    let (status, answer) = adopt(&o, new).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(rows(&o).await, 2);
    assert_eq!(
        delivery_target(&o, 2).await.unwrap().default_branch,
        "trunk"
    );
}

#[tokio::test]
async fn ambiguous_historical_rows_cannot_supply_a_delivery_target_or_be_adopted() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (status, answer) = onboard(&o, request(&o, op)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["disposition"], "bound");
    let duplicate = Uuid::new_v4();
    sqlx::query("INSERT INTO repository_onboardings SELECT $2,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms+1 FROM repository_onboardings WHERE operation_id=$1")
        .bind(op).bind(duplicate).execute(&o.env.pool).await.unwrap();
    assert!(delivery_target(&o, 1).await.is_none());
    sqlx::query(
        "UPDATE repository_onboardings SET state=state-'binding_generation' WHERE operation_id=$1",
    )
    .bind(duplicate)
    .execute(&o.env.pool)
    .await
    .unwrap();
    assert!(delivery_target(&o, 1).await.is_none());
    assert_eq!(
        adopt(&o, adopt_request(Uuid::new_v4())).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(rows(&o).await, 2);
}
