use super::*;

fn check_registration(f: &Fake) {
    let [p] = f.registers.as_slice() else {
        panic!("expected one register")
    };
    assert_eq!(
        (p["mode"].as_str(), p["remote_url"].as_str()),
        (
            Some("central"),
            Some("ssh://git@github.com/acme/widgets.git")
        )
    );
    for field in ["allowed_refs", "allowed_targets"] {
        assert_eq!(p[field], json!(["refs/heads/trunk"]), "{field}");
    }
    // Spec 0016 §5.2: every task's branch is a prefix-scoped target.
    assert_eq!(p["allowed_target_prefixes"], json!(["refs/heads/aq/"]));
    assert!(p.get("credential_profile").is_none());
    assert_eq!(f.syncs[0]["refs"], json!(["refs/heads/trunk"]));
}
fn check_project(project: &Value) {
    let binding = &project["binding"];
    assert_eq!(project["revision"], 1, "{project}");
    assert_eq!(binding["repository_id"], Uuid::from_u128(2).to_string());
    assert_eq!(binding["repository_binding_generation"], 1);
    for (field, expected) in [
        ("repository_adapter_reference", "aq.git.configured"),
        ("graph_adapter_reference", "aq.graph.configured"),
    ] {
        assert_eq!(binding[field], expected);
    }
    assert_eq!(
        (
            binding["publication"]["mode"].as_str(),
            binding["publication"]["target"].as_str()
        ),
        (Some("fast_forward"), Some("refs/heads/trunk"))
    );
}
type Case = (&'static str, &'static str, Box<dyn Fn(&mut Fake)>, bool);
fn refusal_cases() -> Vec<Case> {
    let entry = |mode: &str, kind: &str, name: &str| json!({"mode":mode,"type":kind,"oid":{"algorithm":"sha1","hex":HEAD},"path":{"encoding":"utf8","value":name}});

    vec![
        (
            "gitattributes",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("100644", "blob", ".gitattributes"))),
            false,
        ),
        (
            "submodules",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("160000", "commit", "vendor"))),
            false,
        ),
        (
            "lfs",
            "precheck",
            Box::new(move |f: &mut Fake| f.tree.push(entry("100644", "blob", ".lfsconfig"))),
            false,
        ),
        (
            "required_signing",
            "precheck",
            Box::new(|f: &mut Fake| f.signing = true),
            false,
        ),
        (
            "credential_profile_unavailable",
            "aq-git",
            Box::new(|_: &mut Fake| {}),
            true,
        ),
    ]
}
#[tokio::test]
async fn onboard_github_repo_creates_binding_and_retained_base() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let (status, v) = onboard(&o, request(&o, op)).await;
    assert_eq!(
        (
            status,
            v["disposition"].as_str(),
            v["step"].as_str(),
            v["default_branch"].as_str()
        ),
        (StatusCode::OK, Some("bound"), Some("bound"), Some("trunk")),
        "{v}"
    );
    assert_eq!(
        (
            v["base"]["ref"].as_str(),
            v["base"]["oid"].as_str(),
            v["base"]["observation_id"].as_str()
        ),
        (Some("refs/heads/trunk"), Some(HEAD), Some("obs-1"))
    );
    check_registration(&o.fake.lock().unwrap());
    let (status, project) = call(&o.app, &path(), None, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{project}");
    check_project(&project);
    // Replay of the same operation is the same durable answer, not a second binding.
    let (status, again) = onboard(&o, request(&o, op)).await;
    assert_eq!((status, &again), (StatusCode::OK, &v));
    assert_eq!(o.fake.lock().unwrap().registers.len(), 1);
    let (status, shown) = call(
        &o.app,
        &format!("/api/repositories/onboard/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!((status, shown), (StatusCode::OK, v));
}

#[tokio::test]
async fn unsupported_repo_profile_refused_with_reason() {
    let o = onboarding().await;
    for (reason, source, arrange, credential) in refusal_cases() {
        {
            let mut f = o.fake.lock().unwrap();
            f.tree.truncate(1);
            f.signing = false;
            arrange(&mut f);
        }
        let mut body = request(&o, Uuid::new_v4());
        if credential {
            body["transport"] = "https".into();
            body["credential_profile"] = "acme-token".into();
        }
        let (status, v) = onboard(&o, body.clone()).await;
        assert_eq!(
            (status, v["disposition"].as_str(), v["step"].as_str()),
            (StatusCode::OK, Some("refused"), Some("refused")),
            "{reason}: {v}"
        );
        assert_eq!(
            (
                v["refusal"]["reason"].as_str(),
                v["refusal"]["source"].as_str()
            ),
            (Some(reason), Some(source)),
            "{v}"
        );
        assert!(!v.to_string().contains(SECRET));
        // A refusal is terminal and replayable.
        assert_eq!(onboard(&o, body).await.1, v);
        let (status, _) = call(&o.app, &path(), None, "owner@localhost").await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{reason}: no half-created project"
        );
    }
    let rows: Vec<String> = sqlx::query_scalar("SELECT state::text FROM repository_onboardings")
        .fetch_all(&o.env.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| !r.contains(SECRET)));
    assert!(rows.iter().any(|r| r.contains("acme-token")));
}

#[tokio::test]
async fn onboarding_resumes_after_partial_failure() {
    let o = onboarding().await;
    let op = Uuid::new_v4();
    let mut body = request(&o, op);
    body["default_branch"] = "release".into();
    o.fake.lock().unwrap().branch = "release".into();
    // The graph is unavailable, so only the project-binding step fails.
    o.env
        .graph_status
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = onboard(&o, body.clone()).await;
    assert_eq!(
        (status, v["disposition"].as_str(), v["step"].as_str()),
        (StatusCode::OK, Some("incomplete"), Some("checked")),
        "{v}"
    );
    assert!(v["error"]["code"].is_string());
    o.env
        .graph_status
        .store(0, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = onboard(&o, body.clone()).await;
    assert_eq!(
        (
            status,
            v["disposition"].as_str(),
            v["default_branch"].as_str()
        ),
        (StatusCode::OK, Some("bound"), Some("release")),
        "{v}"
    );
    assert_eq!(o.fake.lock().unwrap().registers.len(), 1);
    body["github"]["name"] = "other".into();
    let (status, _) = onboard(&o, body).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
