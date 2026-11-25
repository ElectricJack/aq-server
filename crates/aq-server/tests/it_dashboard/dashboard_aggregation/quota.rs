use super::*;

#[tokio::test]
async fn quota_unknown_is_not_zero() {
    let mut f = common::profiled::fixture().await;
    unavailable_accounts(&mut f);
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'quota-viewer@localhost','viewer','dev','quota-viewer@localhost',now())")
        .bind(uuid::Uuid::new_v4()).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let viewer_response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/provider-usage")
                .header("x-aq-dev-email", "quota-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_response.status(), StatusCode::FORBIDDEN);
    let (status, body) = read(&app, "/api/v1/provider-usage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let accounts = body["data"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    let usage = &accounts[0];
    assert_eq!(usage["scope"], "provider_account");
    let window = &usage["windows"][0];
    assert_eq!(window["state"], "unknown");
    for field in ["used_percent", "used", "limit"] {
        assert!(window[field].is_null(), "{field}: {window}");
    }
    // QA SO-08: a just-observed unknown window is not already stale.
    let observed = &window["observation"];
    let at = |k: &str| chrono::DateTime::parse_from_rfc3339(observed[k].as_str().unwrap()).unwrap();
    assert_eq!(
        (at("fresh_until") - at("observed_at")).num_milliseconds(),
        5000,
        "{observed}"
    );
    assert!(!body.to_string().contains("private-search-canary"));
    let (_, other) = read(
        &app,
        &format!(
            "/api/v1/provider-usage?scope=project:{}",
            uuid::Uuid::new_v4()
        ),
    )
    .await;
    assert_eq!(body["data"], other["data"]);
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "provider.quota" && v["enabled"] == true)
    );
}

fn unavailable_accounts(f: &mut common::profiled::Fixture) {
    use aq_selection::{ArtifactRef, AuthorizedCredential, Text};
    use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
    use std::sync::Arc;
    let mut config = (*f.state.config).clone();
    let credential = AuthorizedCredential {
        allowed_backends: aq_selection::default_allowed_backends(),
        profile: ArtifactRef {
            id: Text::new("profile").unwrap(),
            version: Text::new("1").unwrap(),
            digest: aq_admission::Digest::of(b"public"),
        },
        provider: "unsupported".into(),
        account_reference: "account".into(),
        endpoint_reference: "private-search-canary".into(),
        revision: 1,
        enabled: true,
        provisionable: false,
        material_owner: None,
    };
    let mut second = credential.clone();
    second.profile.id = Text::new("other-profile").unwrap();
    let credentials = [credential, second]
        .into_iter()
        .map(|credential| CredentialConfig {
            kind: aq_selection::CredentialKind::ApiKey,
            allowed_backends: aq_selection::default_allowed_backends(),
            credential,
            allowed_node_ids: vec![],
            allow_enrolled_nodes: false,
            secret_file: None,
            delivery_mode: None,
        })
        .collect();
    config.intelligence = Some(Arc::new(
        IntelligenceConfig {
            revision: 1,
            credentials,
            ..Default::default()
        }
        .load()
        .unwrap(),
    ));
    f.state.config = Arc::new(config);
}
