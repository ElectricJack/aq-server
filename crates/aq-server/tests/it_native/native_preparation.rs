//! Existing sealed synthetic authority -> actual preparation owner. This is not
//! full native offer assembly or physical host/provider qualification.
use crate::common;
use aq_server::profiled_execution::preparation;
use common::native_preparation::{context, setup};
use uuid::Uuid;
#[tokio::test]
async fn actual_original_plan_lease_mapping_reserved_journal_and_exact_replay_without_grant() {
    let c = setup().await;
    let auth = context(&c).await;
    let nonce = Uuid::new_v4();
    let first = preparation::accept(&c.f.state, &auth, &c.request, nonce)
        .await
        .unwrap();
    first.validate_for(&c.request, nonce).unwrap();
    assert_eq!(first.original.materials.len(), 1);
    c.f.time.fetch_add(500, std::sync::atomic::Ordering::SeqCst);
    let second = preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first.original).unwrap(),
        serde_json::to_value(&second.original).unwrap()
    );
    assert_eq!(
        first.delivery.clock.expires_at_ms,
        second.delivery.clock.expires_at_ms
    );
    assert!(second.delivery.clock.valid_for_ms < first.delivery.clock.valid_for_ms);
    c.f.time
        .fetch_add(10_001, std::sync::atomic::Ordering::SeqCst);
    assert!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
            .await
            .is_err()
    );
    c.f.time
        .fetch_sub(10_001, std::sync::atomic::Ordering::SeqCst);
    let pool = c.f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM admission_material_leases WHERE state='admitted'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_launch_materials")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let phase: String = sqlx::query_scalar("SELECT phase FROM profiled_owner_journals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "reserved");
    let state: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state["status"], "preparing");
    assert!(state["grant"].is_null());
    let mut changed = c.request.clone();
    changed.launch_id = Uuid::new_v4();
    assert!(
        preparation::accept(&c.f.state, &auth, &changed, Uuid::new_v4())
            .await
            .is_err()
    );
}

async fn post(
    c: &common::native_preparation::Case,
    path: &str,
    body: Vec<u8>,
    nonce: Option<Uuid>,
    admin: bool,
) -> axum::response::Response {
    use tower::ServiceExt;
    let mut request = axum::http::Request::post(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", c.f.credential));
    if admin {
        request = request
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1");
    }
    if let Some(nonce) = nonce {
        request = request.header("aq-delivery-nonce", nonce.to_string());
    }
    let app = if admin {
        aq_server::build_admin_router(c.f.state.clone())
    } else {
        aq_server::build_worker_router(c.f.state.clone())
    };
    app.oneshot(request.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap()
}
#[tokio::test]
async fn mounted_accept_and_actual_private_material_fetch_keep_preparation_separate_from_grant() {
    use aq_admission::{
        control::{ControlReply, decode_reply},
        transport::MaterialFetch,
    };
    use http_body_util::BodyExt;
    let c = setup().await;
    let nonce = Uuid::new_v4();
    let body = serde_json::to_vec(&c.request).unwrap();
    assert_eq!(
        post(
            &c,
            "/v1/worker/profiled/accept",
            body.clone(),
            Some(nonce),
            true
        )
        .await
        .status(),
        404
    );
    let response = post(&c, "/v1/worker/profiled/accept", body, Some(nonce), false).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let ControlReply::Preparation(plan) =
        decode_reply(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    else {
        panic!()
    };
    plan.validate_for(&c.request, nonce).unwrap();
    let request = MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.acceptance.binding.clone(),
        lease_id: plan.original.materials[0].lease_id,
    };
    let response = post(
        &c,
        "/v1/worker/profiled/material",
        serde_json::to_vec(&request).unwrap(),
        None,
        false,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("aq-material-delivery"));
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"synthetic-not-a-provider-key"
    );
    let state: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&c.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(state["status"], "preparing");
    assert!(state["grant"].is_null());
}
#[tokio::test]
async fn accept_missing_nonce_unknown_and_duplicate_fields_and_body_bound_are_typed_failures() {
    use aq_admission::control::{ControlReply, decode_reply};
    use http_body_util::BodyExt;
    let c = setup().await;
    let value = serde_json::to_value(&c.request).unwrap();
    let original = serde_json::to_vec(&value).unwrap();
    let mut unknown = value.clone();
    unknown["chosen_account"] = "never-accepted".into();
    let text = String::from_utf8(original.clone()).unwrap();
    let duplicate = text
        .replacen(
            "\"operation_id\":",
            &format!(
                "\"operation_id\":\"{}\",\"operation_id\":",
                c.request.operation_id
            ),
            1,
        )
        .into_bytes();
    for (body, nonce, status) in [
        (original, None, 400),
        (
            serde_json::to_vec(&unknown).unwrap(),
            Some(Uuid::new_v4()),
            400,
        ),
        (duplicate, Some(Uuid::new_v4()), 400),
        (vec![b'x'; 65537], Some(Uuid::new_v4()), 413),
    ] {
        let response = post(&c, "/v1/worker/profiled/accept", body, nonce, false).await;
        assert_eq!(response.status(), status);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() < 1024);
        assert!(matches!(
            decode_reply(&bytes).unwrap(),
            ControlReply::Failure(_)
        ));
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&c.f.db.pool().await)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn changed_private_file_never_admits_lease_or_reservation_journal() {
    let c = setup().await;
    let auth = context(&c).await;
    let cfg = c.f.state.config.intelligence.as_ref().unwrap();
    let credential = cfg
        .config
        .credentials
        .iter()
        .find(|v| v.credential.profile == c.f.selection.choice.credential_profile)
        .unwrap();
    std::fs::write(
        credential.secret_file.as_ref().unwrap(),
        b"changed private bytes",
    )
    .unwrap();
    assert!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
            .await
            .is_err()
    );
    let pool = c.f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_owner_journals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let state: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state["status"], "offered");
}
#[tokio::test]
async fn expiry_after_observed_node_lock_wait_never_commits_preparation() {
    let c = setup().await;
    let auth = context(&c).await;
    let pool = c.f.db.pool().await;
    let mut blocker = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(c.f.node.node_id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let accept = preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4());
    let expire = async {
        tokio::time::timeout(std::time::Duration::from_secs(1),async {loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%credential_hash%' AND wait_event_type='Lock')").bind(pid).fetch_one(&pool).await.unwrap();
            if waiting {break} tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }}).await.unwrap();
        c.f.time
            .fetch_add(30_001, std::sync::atomic::Ordering::SeqCst);
        blocker.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(accept, expire);
    assert!(result.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn credential_rotation_during_actual_git_io_never_admits_preparation() {
    let c = setup().await;
    let auth = context(&c).await;
    let pool = c.f.db.pool().await;
    c.pause.store(true, std::sync::atomic::Ordering::SeqCst);
    let accept = preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4());
    let rotate = async {
        tokio::time::timeout(std::time::Duration::from_secs(3), c.entered.notified())
            .await
            .unwrap();
        sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
            .bind(c.f.node.node_id)
            .bind("rotated-current-machine-credential")
            .execute(&pool)
            .await
            .unwrap();
        c.resume.notify_one();
    };
    let (result, ()) = tokio::join!(accept, rotate);
    assert_eq!(
        result.unwrap_err().status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn issued_grant_cannot_be_discounted_as_an_owned_ungranted_reservation() {
    let c = setup().await;
    let auth = context(&c).await;
    preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
        .await
        .unwrap();
    let pool = c.f.db.pool().await;
    let raw: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut hot: aq_server::execution::authority::hot::HotState =
        serde_json::from_value(raw).unwrap();
    let now = c.f.time.load(std::sync::atomic::Ordering::SeqCst) as u64;
    hot.status = aq_server::execution::profiled::Status::Granted;
    hot.execution_deadline_ms = Some(now + 30000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: c.request.acceptance.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: now + 1000,
    });
    // Synthetic admitted-grant fixture only; no physical start is inferred.
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(c.request.acceptance.binding.attempt_id)
        .bind(serde_json::json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn journal_storage_failure_rolls_back_lease_mapping_hot_and_receipt_atomically() {
    let c = setup().await;
    let auth = context(&c).await;
    let pool = c.f.db.pool().await;
    sqlx::raw_sql("CREATE FUNCTION fixture_reject_reserved() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic journal storage failure'; END $$; CREATE TRIGGER fixture_reject_reserved BEFORE INSERT ON profiled_owner_journals FOR EACH ROW EXECUTE FUNCTION fixture_reject_reserved();").execute(&pool).await.unwrap();
    assert!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
            .await
            .is_err()
    );
    for table in [
        "admission_material_leases",
        "profiled_launch_materials",
        "profiled_owner_journals",
        "profiled_execution_receipts",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "{table}"
        );
    }
    let state: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state["status"], "offered");
    assert!(state["owner"].is_null());
}
#[tokio::test]
async fn concurrent_exact_accept_can_reconcile_original_receipt_without_duplicate_leases() {
    let c = setup().await;
    let auth = context(&c).await;
    let (a, b) = tokio::join!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4()),
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
    );
    assert!(
        a.is_ok() || b.is_ok(),
        "at least one bounded admission must win"
    );
    let original = a.ok().or_else(|| b.ok()).unwrap();
    // Explicit original-ID reconciliation after concurrency/serialization refusal;
    // no transport retry or new mutation ID is hidden inside the service.
    let reconciled = preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(original.original).unwrap(),
        serde_json::to_value(reconciled.original).unwrap()
    );
    let pool = c.f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_owner_journals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='accept'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn extra_known_worker_capabilities_and_missing_durable_journal_never_qualify_preparation() {
    for (qualified_caps, registered_journal) in [(false, true), (true, false)] {
        let c =
            common::native_preparation::setup_with_capabilities(qualified_caps, registered_journal)
                .await;
        let auth = context(&c).await;
        assert!(
            preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
                .fetch_one(&c.f.db.pool().await)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn incoming_preparation_bytes_cannot_overshoot_retained_storage_limit() {
    let c = setup().await;
    let auth = context(&c).await;
    let pool = c.f.db.pool().await;
    // Unknown historical response bytes are conservative storage charges, never
    // acknowledgement/authority fixtures. No current operation can replay them.
    let mut remaining = 4_194_304usize - 1;
    while remaining > 0 {
        let bytes = remaining.min(200_000);
        assert!(bytes >= 2);
        sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)")
            .bind(c.f.node.node_id).bind(Uuid::new_v4()).bind(aq_admission::Digest::of(b"synthetic storage-only unknown history").as_str()).bind(serde_json::json!("x".repeat(bytes-2))).execute(&pool).await.unwrap();
        remaining -= bytes;
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        4_194_303
    );
    assert!(
        preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
            .await
            .is_err(),
        "existing bytes are below the cap but the incoming lease/plan do not fit"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_owner_journals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn swarm_native_offer_reserves_class_and_lowering_limit_preserves_preparation() {
    use aq_swarm_settings::admission::{self, CEILING};
    let c = setup().await;
    let pool = c.f.db.pool().await;
    let decision: serde_json::Value =
        sqlx::query_scalar("SELECT decision FROM intelligence_decisions WHERE decision_id=$1")
            .bind(c.f.decision)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let cell = admission::resolve(
        &mut conn,
        decision["inputs"]["project"]["catalog_digest"].as_str(),
        decision["inputs"]["project"]["intelligence_profile"].as_str(),
        &decision["outcome"]["resolved"]["choice"],
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton").bind(serde_json::json!([{"provider":cell.provider,"intelligence_class":cell.intelligence_class,"limit":1}])).execute(&mut *conn).await.unwrap();
    // The committed offer consumes the one slot before any process starts.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        admission::check(&mut tx, Some(&cell)).await.unwrap(),
        Some(CEILING)
    );
    tx.rollback().await.unwrap();
    let counts = admission::counts(&mut conn).await.unwrap();
    assert_eq!(counts.iter().find(|x|x.provider==cell.provider && x.intelligence_class==cell.intelligence_class).unwrap().running,1);
    // A zero ceiling applies only to new admissions. The offered reservation
    // remains valid for its owner and may continue through preparation.
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton").bind(serde_json::json!([{"provider":cell.provider,"intelligence_class":cell.intelligence_class,"limit":0}])).execute(&mut *conn).await.unwrap();
    drop(conn);
    let auth = context(&c).await;
    preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    // The pill's named class comes from the immutable catalog used to admit
    // this attempt, even if the owner's active routing catalog later changes.
    let mut swarm: aq_dashboard_contract::Swarm = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../../protocol/dashboard-read/v1/fixtures/swarm.json"
        ))
        .unwrap()["data"]
            .clone(),
    )
    .unwrap();
    let agent = swarm
        .machines
        .iter_mut()
        .flat_map(|m| &mut m.nodes)
        .flat_map(|n| &mut n.agents)
        .next()
        .unwrap();
    agent.attempt_id = c.f.profiled.offer.binding.attempt_id;
    sqlx::query("UPDATE profile_catalog_registry SET active_digest=NULL WHERE singleton")
        .execute(&mut *conn)
        .await
        .unwrap();
    aq_swarm::routing::apply(&mut conn, &mut swarm)
        .await
        .unwrap();
    let routing = swarm
        .machines
        .iter()
        .flat_map(|m| &m.nodes)
        .flat_map(|n| &n.agents)
        .find(|a| a.attempt_id == c.f.profiled.offer.binding.attempt_id)
        .unwrap()
        .routing_profile
        .as_ref()
        .unwrap();
    assert_eq!(routing.id, cell.intelligence_class);
    assert!(!routing.display_name.is_empty());
}
