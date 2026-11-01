//! Actual030 no-launch owner/SQL with synthetic Graph/Git/runtime qualification.
//! No execution grant, host, provider or ordered030 migration claim.
mod common;
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Resources, Text,
    control::{ControlReply, decode_reply},
    preparation_v2::{
        self as phase, AcceptRequest, FactKind, OwnerAuthority, PhasePolicy, ResourceHold,
    },
};
use aq_server::{
    profiled_execution::{
        accepted_preparation::{self as owner, PolicyDefinition, ProposalRequest},
        definitions::Definition,
        policies,
    },
    worker_api,
};
use common::native_offer::{self, Case};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
async fn configure(c: &mut Case) {
    let mut config = (*c.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let r = &p.qualified_runtimes[0];
    let policy = PolicyDefinition {
        schema_version: 1,
        contract: phase::contract_artifact().unwrap(),
        runtime: r.runtime.clone(),
        qualification: r.qualification.clone(),
        phase: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 10000,
            maximum_execution_lease_ms: 10000,
            maximum_observation_ms: 2000,
        },
        source_profile: reference("synthetic-source-profile"),
        source_disk_bytes: 1 << 20,
        owners: FactKind::all()
            .iter()
            .map(|k| OwnerAuthority {
                kind: *k,
                authority: reference(&format!("synthetic-owner/{k:?}")),
            })
            .collect(),
    };
    let body = serde_json::to_string(&policy).unwrap();
    let a = ArtifactRef {
        id: Text::new("aq.accepted-preparation-policy/synthetic-test").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    p.definition_bodies
        .push(Definition::new(a, ArtifactKind::Definition, body).unwrap());
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(std::sync::Arc::new(p));
    c.env.env.state.config = std::sync::Arc::new(config);
    policies::install(&c.env.env.state).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0030_accepted_preparation_v2.sql"
    ))
    .execute(&c.env.env.pool)
    .await
    .unwrap();
}
async fn auth(c: &Case) -> worker_api::AuthenticatedNode {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    worker_api::authenticate_context(&c.env.env.state, &h)
        .await
        .unwrap()
}
async fn setup() -> (Case, phase::Proposal, AcceptRequest) {
    let mut c = native_offer::setup().await;
    configure(&mut c).await;
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    let offer = page.offers.remove(0);
    let negotiation = phase::Negotiation {
        contract: phase::contract_artifact().unwrap(),
        features: phase::required_features(),
    };
    let q = ProposalRequest {
        identity: c.request.identity.clone(),
        binding: offer.offered.offer.binding.clone(),
        negotiation: negotiation.clone(),
    };
    let p = owner::propose(&c.env.env.state, &auth(&c).await, &q)
        .await
        .unwrap();
    let mut original = native_offer::acceptance(&c, &offer);
    original.reservation_remaining_ms = 1000;
    let request = AcceptRequest {
        schema_version: 2,
        negotiation,
        proposal_digest: p.digest().unwrap(),
        hold: ResourceHold {
            maximum_preparation_ms: 10000,
            reservation: original.reservation.clone(),
            resources: Resources {
                slots: 1,
                memory_bytes: offer.offered.offer.manifest.resources.memory_bytes,
                cpu_millis: offer.offered.offer.manifest.resources.cpu_millis,
                disk_bytes: 1 << 20,
            },
            source_profile: reference("synthetic-source-profile"),
            source_disk_bytes: 1 << 20,
        },
        original,
    };
    (c, p, request)
}
#[tokio::test]
async fn real_timed_accept_retains_independent_lease_and_exact_replay() {
    let (c, p, q) = setup().await;
    let auth = auth(&c).await;
    let a = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    a.validate_for(&q, &p, a.delivery.nonce).unwrap();
    assert!(a.receipt.lease.expires_at_ms > p.original.offered.clock.expires_at_ms);
    assert!(
        a.receipt.lease.expires_at_ms - a.receipt.lease.issued_at_ms
            > q.original.reservation_remaining_ms
    );
    let replay = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(a.receipt).unwrap(),
        serde_json::to_value(replay.receipt).unwrap()
    );
    let row=sqlx::query("SELECT (SELECT count(*) FROM admission_material_leases) AS leases,(SELECT count(*) FROM profiled_v2_accepts) AS receipts,(SELECT count(*) FROM profiled_execution_receipts WHERE kind='accept') AS legacy").fetch_one(&c.env.env.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("leases"), 1);
    assert_eq!(row.get::<i64, _>("receipts"), 1);
    assert_eq!(row.get::<i64, _>("legacy"), 0);
    let hot: Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "preparing");
    assert!(hot["grant"].is_null());
}
#[tokio::test]
async fn absent_policy_has_no_route_and_invalid_policy_refuses_configuration() {
    let c = native_offer::setup().await;
    let response = native_offer::post(
        &c.env.env.state,
        &c.env.env.credential,
        "/v1/worker/profiled-v2/proposal",
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(response.status(), 404);
    let mut c = c;
    configure(&mut c).await;
    let config = c.env.env.state.config.profiled_execution.as_ref().unwrap();
    let def = config
        .definition_bodies
        .iter()
        .find(|d| {
            d.artifact()
                .id
                .as_str()
                .starts_with("aq.accepted-preparation-policy/")
        })
        .unwrap();
    let policy: PolicyDefinition = serde_json::from_str(def.body()).unwrap();
    for mode in 0..5 {
        let mut p = policy.clone();
        match mode {
            0 => p.source_disk_bytes = 0,
            1 => p.contract.digest = Digest::of(b"unsupported"),
            2 => p.owners[1] = p.owners[0].clone(),
            3 => p.phase.maximum_preparation_ms = 120001,
            _ => p.schema_version = 2,
        };
        assert!(p.validate().is_err());
    }
}
#[tokio::test]
async fn mixed_candidate_schema_is_corruption_not_older_profile() {
    let (c, p, q) = setup().await;
    sqlx::query("DROP TABLE profiled_v2_accepts")
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    assert!(
        owner::accept(&c.env.env.state, &auth(&c).await, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    let request = ProposalRequest {
        identity: c.request.identity.clone(),
        binding: p.original.offered.offer.binding,
        negotiation: p.negotiation,
    };
    assert!(
        owner::propose(&c.env.env.state, &auth(&c).await, &request)
            .await
            .is_err()
    );
}
async fn artifact(c: &Case, q: &AcceptRequest, p: &phase::Proposal) -> axum::response::Response {
    let d = &p.original.offered.artifacts[0];
    let fetch = aq_admission::transport::ArtifactFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: q.original.identity.clone(),
        binding: q.original.acceptance.binding.clone(),
        artifact: d.artifact.clone(),
        offset: 0,
        length: d.bytes.min(1024) as u32,
    };
    native_offer::post(
        &c.env.env.state,
        &c.env.env.credential,
        "/v1/worker/profiled/artifact",
        serde_json::to_vec(&fetch).unwrap(),
    )
    .await
}
async fn source(c: &Case, q: &AcceptRequest) -> axum::response::Response {
    use tower::ServiceExt;
    aq_server::build_worker_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::get(format!(
                "/v1/worker/profiled/source/{}",
                q.original.acceptance.binding.attempt_id
            ))
            .header("authorization", format!("Bearer {}", c.env.env.credential))
            .header("aq-source-api-version", "1")
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn original_inputs_remain_scoped_after_proposal_expiry_then_expire_held() {
    let (mut c, p, q) = setup().await;
    let a = owner::accept(&c.env.env.state, &auth(&c).await, &q, Uuid::new_v4())
        .await
        .unwrap();
    let time = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        p.original.offered.clock.expires_at_ms + 1,
    ));
    let at = time.clone();
    c.env.env.state.clock = std::sync::Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(at.load(std::sync::atomic::Ordering::SeqCst) as i64)
            .unwrap()
    });
    let auth = auth(&c).await;
    assert_eq!(artifact(&c, &q, &p).await.status(), 200);
    assert_eq!(source(&c, &q).await.status(), 200);
    let fetch = aq_admission::transport::MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: q.original.identity.clone(),
        binding: q.original.acceptance.binding.clone(),
        lease_id: a.receipt.lease.materials[0].lease_id,
    };
    let material =
        aq_server::profiled_execution::material_api::fetch(&c.env.env.state, &auth, &fetch)
            .await
            .unwrap();
    assert_eq!(
        Digest::of(material.bytes()),
        a.receipt.lease.materials[0].material_digest
    );
    owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    assert!(
        aq_server::profiled_execution::preparation::accept(
            &c.env.env.state,
            &auth,
            &q.original,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    time.store(
        a.receipt.lease.expires_at_ms,
        std::sync::atomic::Ordering::SeqCst,
    );
    assert!(
        owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_ne!(artifact(&c, &q, &p).await.status(), 200);
    assert_ne!(source(&c, &q).await.status(), 200);
    assert!(
        aq_server::profiled_execution::material_api::fetch(&c.env.env.state, &auth, &fetch)
            .await
            .is_err()
    );
    assert!(
        aq_server::profiled_execution::no_grant::close_expired(
            &c.env.env.state,
            q.original.identity.node_id,
            q.original.acceptance.binding.attempt_id
        )
        .await
        .is_err()
    );
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(q.original.acceptance.binding.attempt_id)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert!(held);
}
#[tokio::test]
async fn changed_bytes_runtime_scope_and_cancel_refuse_without_new_receipts() {
    let (c, p, q) = setup().await;
    let auth = auth(&c).await;
    for mode in 0..4 {
        let mut bad = q.clone();
        match mode {
            0 => bad.hold.source_profile = reference("foreign-profile"),
            1 => bad.hold.source_disk_bytes += 1,
            2 => bad.original.identity.boot_id = Uuid::new_v4(),
            _ => bad.proposal_digest = Digest::of(b"foreign"),
        };
        assert!(
            owner::accept(&c.env.env.state, &auth, &bad, Uuid::new_v4())
                .await
                .is_err()
        );
    }
    let first = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    let versions:(i64,i64,i64)=sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(q.original.acceptance.binding.attempt_id).fetch_one(&c.env.env.pool).await.unwrap();
    assert_eq!(
        (
            first.receipt.versions.record_revision,
            first.receipt.versions.authority_revision,
            first.receipt.versions.grant_revision
        ),
        (versions.0 as u64, versions.1 as u64, versions.2 as u64)
    );
    let mut bad = q.clone();
    bad.original.launch_id = Uuid::new_v4();
    assert!(
        owner::accept(&c.env.env.state, &auth, &bad, Uuid::new_v4())
            .await
            .is_err()
    );
    let mut bad = q.clone();
    bad.original.operation_id = Uuid::new_v4();
    assert!(
        owner::accept(&c.env.env.state, &auth, &bad, Uuid::new_v4())
            .await
            .is_err()
    );
    // Synthetic owner-state mutation, not physical stop or control-route evidence.
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{cancellation_id}',to_jsonb($2::text)) WHERE attempt_id=$1").bind(q.original.acceptance.binding.attempt_id).bind(Uuid::new_v4().to_string()).execute(&c.env.env.pool).await.unwrap();
    assert!(
        owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_ne!(artifact(&c, &q, &p).await.status(), 200);
    assert_ne!(source(&c, &q).await.status(), 200);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_v2_accepts")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
async fn fill_retained_quota(c: &Case) {
    // Finite retained synthetic historical payloads; no fabricated live authority.
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) SELECT $1,md5(n::text)::uuid,'cancel',repeat('a',64),jsonb_build_object('retained_history',repeat('x',262000)) FROM generate_series(1,16) n").bind(c.request.identity.node_id).execute(&c.env.env.pool).await.unwrap();
}
#[tokio::test]
async fn shared_bytes_quota_rolls_back_new_accept_but_never_forgets_exact_replay() {
    let (c, _, q) = setup().await;
    fill_retained_quota(&c).await;
    assert!(
        owner::accept(&c.env.env.state, &auth(&c).await, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    for table in [
        "admission_material_leases",
        "profiled_v2_accepts",
        "profiled_owner_journals",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let hot: Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "offered");
    drop(c); // selection fixture owns a process-local serial gate.
    let (c, _, q) = setup().await;
    let auth = auth(&c).await;
    let first = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    fill_retained_quota(&c).await;
    let replay = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        first.receipt.digest().unwrap(),
        replay.receipt.digest().unwrap()
    );
}
#[tokio::test]
async fn observed_node_wait_charges_expiry_before_any_new_accept_commit() {
    let (mut c, p, q) = setup().await;
    let time = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        p.original.offered.clock.server_time_ms + 1,
    ));
    let at = time.clone();
    c.env.env.state.clock = std::sync::Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(at.load(std::sync::atomic::Ordering::SeqCst) as i64)
            .unwrap()
    });
    let auth = auth(&c).await;
    let mut blocker = c.env.env.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(q.original.identity.node_id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let pending = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4());
    let expire = async {
        tokio::time::timeout(std::time::Duration::from_secs(1),async{loop{
   let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%credential_hash%' AND wait_event_type='Lock')").bind(pid).fetch_one(&c.env.env.pool).await.unwrap();if waiting{break}tokio::time::sleep(std::time::Duration::from_millis(5)).await;
  }}).await.unwrap();
        time.store(
            p.original.offered.clock.expires_at_ms,
            std::sync::atomic::Ordering::SeqCst,
        );
        blocker.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(pending, expire);
    assert!(result.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admission_material_leases")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn failure_after_actual_material_insert_rolls_back_entire_admission() {
    let (c, _, q) = setup().await;
    sqlx::raw_sql("CREATE FUNCTION test_v2_fail_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic rollback cut after material insert'; END $$; CREATE TRIGGER test_v2_fail BEFORE INSERT ON profiled_v2_accepts FOR EACH ROW EXECUTE FUNCTION test_v2_fail_receipt();").execute(&c.env.env.pool).await.unwrap();
    assert!(
        owner::accept(&c.env.env.state, &auth(&c).await, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    for table in [
        "admission_material_leases",
        "profiled_launch_materials",
        "profiled_v2_accepts",
        "profiled_owner_journals",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let hot: Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "offered");
}
#[tokio::test]
async fn legacy_and_v2_accept_share_one_actual_owner_and_material_scope() {
    let (c, _, q) = setup().await;
    let auth = auth(&c).await;
    let (v2, legacy) = tokio::join!(
        owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4()),
        aq_server::profiled_execution::preparation::accept(
            &c.env.env.state,
            &auth,
            &q.original,
            Uuid::new_v4()
        )
    );
    assert_ne!(
        v2.is_ok(),
        legacy.is_ok(),
        "exactly one admission wins: v2={v2:?} legacy={legacy:?}"
    );
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM admission_material_leases),(SELECT count(*) FROM profiled_v2_accepts)+(SELECT count(*) FROM profiled_execution_receipts WHERE kind='accept')").fetch_one(&c.env.env.pool).await.unwrap();
    assert_eq!(counts, (1, 1));
}
#[tokio::test]
async fn mounted_v2_routes_require_machine_auth_negotiation_and_physical_nonce() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (c, p, q) = setup().await;
    let path = "/v1/worker/profiled-v2/accept";
    let app = aq_server::build_worker_router(c.env.env.state.clone());
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post(path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let body = serde_json::to_vec(&phase::Document::Accept(Box::new(q.clone()))).unwrap();
    let response =
        native_offer::post(&c.env.env.state, &c.env.env.credential, path, body.clone()).await;
    assert_eq!(response.status(), 400);
    let nonce = Uuid::new_v4();
    let response = app
        .oneshot(
            axum::http::Request::post(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", c.env.env.credential))
                .header("aq-delivery-nonce", nonce.to_string())
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let doc = phase::decode(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let phase::Document::AcceptedDelivery(delivery) = doc else {
        panic!("wrong response")
    };
    delivery.validate_for(&q, &p, nonce).unwrap();
    let admin = aq_server::build_admin_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post(path)
                .header("content-type", "application/json")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("authorization", format!("Bearer {}", c.env.env.credential))
                .body(axum::body::Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin.status(), 404);
}
#[tokio::test]
async fn changed_current_project_and_private_bytes_cannot_deliver_inputs() {
    let (c, p, q) = setup().await;
    let auth = auth(&c).await;
    let a = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    let fetch = aq_admission::transport::MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: q.original.identity.clone(),
        binding: q.original.acceptance.binding.clone(),
        lease_id: a.receipt.lease.materials[0].lease_id,
    };
    std::fs::write(
        c.env.env._dir.path().join("synthetic-provider-key"),
        b"changed synthetic private bytes",
    )
    .unwrap();
    assert!(
        aq_server::profiled_execution::material_api::fetch(&c.env.env.state, &auth, &fetch)
            .await
            .is_err()
    );
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(p.spec.lifecycle.project_id)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    assert!(
        owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_ne!(artifact(&c, &q, &p).await.status(), 200);
    assert_ne!(source(&c, &q).await.status(), 200);
}
