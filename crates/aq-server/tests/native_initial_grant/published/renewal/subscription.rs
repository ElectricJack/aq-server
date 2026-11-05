//! C5b renewal over a versioned subscription login: a refreshed revision of the
//! same credential is continuity; an operator replace or rotation is not.
use super::*;
use aq_server::profiled_execution::login_refresh::{
    self as login_refresh,
    api::{HEADER, MaterialRefresh, PATH, RefreshOutcome},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

async fn login_setup() -> Prepared {
    super::super::super::common::selection::SUBSCRIPTION_LOGIN
        .scope(true, setup())
        .await
}
async fn write_back(c: &Prepared, base_version: i64, bytes: &[u8]) -> (StatusCode, Vec<u8>) {
    let lease = &c.plan.materials[0];
    let doc = MaterialRefresh {
        base_version,
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.binding.clone(),
        lease_id: lease.lease_id,
        content_digest: Digest::of(bytes),
    };
    let response = aq_server::build_worker_router(c.case.env.env.state.clone())
        .oneshot(
            Request::post(PATH)
                .header("content-type", "application/octet-stream")
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header(
                    HEADER,
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&doc).unwrap()),
                )
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
}
/// The next renewal, 50 ms later: a renewal at the same instant can never
/// extend the expiry, so it is refused regardless of the login.
fn next(c: &Prepared, r: &Renew) -> Renew {
    c.clock.fetch_add(50, Ordering::SeqCst);
    let mut n = r.clone();
    n.operation_id = Uuid::new_v4();
    n.journal_revision += 1;
    n
}
async fn renewed(c: &Prepared, r: &Renew) {
    let (status, reply) = send(c, r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    assert!(matches!(reply, ControlReply::Renewed(_)), "{reply:?}");
}

#[tokio::test]
async fn refreshed_revision_does_not_refuse_own_renewal() {
    let c = login_setup().await;
    let initial = grant(&c).await.unwrap();
    let first = request(&c, &initial);
    renewed(&c, &first).await;
    // The running login rotates its own tokens twice under this lease.
    for (n, bytes) in [b"refreshed login one".as_slice(), b"refreshed login two"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = write_back(&c, n as i64, bytes).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let outcome: RefreshOutcome = serde_json::from_slice(&body).unwrap();
        assert!(outcome.accepted && !outcome.replayed);
        assert_eq!(outcome.version, n as i64 + 1);
    }
    // Renewal after the refreshes still succeeds (identity, not bytes).
    renewed(&c, &next(&c, &first)).await;
}

#[tokio::test]
async fn operator_rotation_refuses_renewal() {
    // An operator upload (import/replace) becomes the newest version
    // without changing the configured file.
    let c = login_setup().await;
    let initial = grant(&c).await.unwrap();
    let r = request(&c, &initial);
    renewed(&c, &r).await;
    let lease = &c.plan.materials[0];
    let registry = c.case.env.env.state.config.intelligence.clone().unwrap();
    let version = login_refresh::operator_replace(
        &c.case.env.env.pool,
        &registry,
        &lease.credential_profile,
        lease.credential_revision,
        Arc::new(zeroize::Zeroizing::new(b"operator replaced login".to_vec())),
        1,
    )
    .await
    .unwrap();
    assert_eq!(version, 1);
    let (status, reply) = send(&c, &next(&c, &r)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    // Nor can the old lease write back over the operator's version.
    let (status, _) = write_back(&c, 1, b"refreshed after replace").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn configured_file_rotation_refuses_renewal() {
    // The operator replaces the configured login file and reloads. One fixture
    // per test (see `second_live_fixture_is_refused_not_deadlocked`).
    let mut c = login_setup().await;
    let initial = grant(&c).await.unwrap();
    let r = request(&c, &initial);
    renewed(&c, &r).await;
    let registry = c.case.env.env.state.config.intelligence.clone().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|r| r.credential.profile == c.plan.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .clone()
        .unwrap();
    std::fs::write(&path, b"operator rotated login").unwrap();
    let mut config = (*c.case.env.env.state.config).clone();
    config.intelligence = Some(Arc::new(registry.config.clone().load().unwrap()));
    c.case.env.env.state.config = Arc::new(config);
    let (status, reply) = send(&c, &next(&c, &r)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    let (status, _) = write_back(&c, 0, b"refreshed after rotation").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Regression for the C5b hang: `operator_rotation_refuses_renewal` once built
/// a second fixture via `let mut c = login_setup().await` while the shadowed
/// first `c` (holding the process-wide fixture gate) was still alive, and
/// waited on itself for hours. The gate now refuses that at once.
#[tokio::test]
#[should_panic(expected = "fixture gate is not re-entrant")]
async fn second_live_fixture_is_refused_not_deadlocked() {
    let _first = login_setup().await;
    let _second = tokio::time::timeout(std::time::Duration::from_secs(60), login_setup())
        .await
        .expect("second fixture neither refused nor built: gate deadlock");
}
