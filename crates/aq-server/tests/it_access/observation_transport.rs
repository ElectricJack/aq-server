use crate::common;
use aq_observation_admission::{Operation, Phase, Reply, Request, VERSION};
use aq_server::observations::transport;
use common::profiled::{Fixture, fixture};
use serde_json::json;
use uuid::Uuid;
async fn setup() -> Fixture {
    fixture().await
}
fn prepare(f: &Fixture) -> Request {
    Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: f.node.node_id,
        operation: Operation::Prepare {
            boot_id: f.node.boot_id,
            store_id: Uuid::new_v4(),
        },
    }
}
fn activate(f: &Fixture, reply: &Reply) -> Request {
    Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: f.node.node_id,
        operation: Operation::Activate {
            marker: reply.current_owner.marker.clone(),
        },
    }
}
async fn rows(f: &Fixture) -> (i64, i64) {
    let pool = f.db.pool().await;
    (
        sqlx::query_scalar("SELECT count(*) FROM observation_transport_owners")
            .fetch_one(&pool)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM observation_transport_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
}
#[tokio::test]
async fn lost_prepare_and_activation_replies_never_reseed_even_at_zero_cursor() {
    let f = setup().await;
    let p = prepare(&f);
    let first = transport::mutate(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    assert!(first.may_create(&p).unwrap());
    assert_eq!(
        first,
        transport::mutate(&f.state, f.node.node_id, &p)
            .await
            .unwrap()
    );
    assert_eq!(rows(&f).await, (1, 1));
    let a = activate(&f, &first);
    let active = transport::mutate(&f.state, f.node.node_id, &a)
        .await
        .unwrap();
    assert_eq!(active.current_owner.phase, Phase::Active);
    let replay = transport::mutate(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(replay.current_owner, active.current_owner);
    assert!(!replay.may_create(&p).unwrap());
    assert_eq!(
        transport::mutate(&f.state, f.node.node_id, &a)
            .await
            .unwrap(),
        active
    );
    assert!(
        transport::mutate(&f.state, f.node.node_id, &prepare(&f))
            .await
            .is_err()
    );
    let status = transport::status(&f.state, f.node.node_id).await.unwrap();
    assert_eq!(status.owner, Some(active.current_owner));
    assert_eq!(status.accepted_through, 0);
    assert_eq!(rows(&f).await, (1, 2));
}
#[tokio::test]
async fn concurrent_prepares_choose_one_owner_and_same_id_changed_bytes_conflict() {
    let f = setup().await;
    let a = prepare(&f);
    let b = prepare(&f);
    let (x, y) = tokio::join!(
        transport::mutate(&f.state, f.node.node_id, &a),
        transport::mutate(&f.state, f.node.node_id, &b)
    );
    assert_ne!(x.is_ok(), y.is_ok());
    assert_eq!(rows(&f).await, (1, 1));
    let selected = if x.is_ok() { a } else { b };
    let mut changed = selected.clone();
    if let Operation::Prepare { store_id, .. } = &mut changed.operation {
        *store_id = Uuid::new_v4();
    }
    assert!(
        transport::mutate(&f.state, f.node.node_id, &changed)
            .await
            .is_err()
    );
    let (x, y) = tokio::join!(
        transport::mutate(&f.state, f.node.node_id, &selected),
        transport::mutate(&f.state, f.node.node_id, &selected)
    );
    assert_eq!(x.unwrap(), y.unwrap());
    assert_eq!(rows(&f).await, (1, 1));
}
#[tokio::test]
async fn receipt_failure_rolls_back_owner_and_monotonic_guards_refuse_resurrection() {
    let f = setup().await;
    let p = prepare(&f);
    let first = transport::mutate(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    let pool = f.db.pool().await;
    sqlx::raw_sql("CREATE FUNCTION test_fail_activation() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN IF NEW.owner_revision=2 THEN RAISE EXCEPTION 'synthetic receipt failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_fail_activation BEFORE INSERT ON observation_transport_receipts FOR EACH ROW EXECUTE FUNCTION test_fail_activation();").execute(&pool).await.unwrap();
    let a = activate(&f, &first);
    assert!(
        transport::mutate(&f.state, f.node.node_id, &a)
            .await
            .is_err()
    );
    assert_eq!(rows(&f).await, (1, 1));
    assert_eq!(
        transport::status(&f.state, f.node.node_id)
            .await
            .unwrap()
            .owner,
        Some(first.current_owner.clone())
    );
    sqlx::query("DROP TRIGGER test_fail_activation ON observation_transport_receipts")
        .execute(&pool)
        .await
        .unwrap();
    let active = transport::mutate(&f.state, f.node.node_id, &a)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE observation_transport_owners SET state=$1")
            .bind(json!(first.current_owner))
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM observation_transport_owners")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE observation_transport_receipts SET receipt=receipt")
            .execute(&pool)
            .await
            .is_err()
    );
    assert_eq!(
        transport::status(&f.state, f.node.node_id)
            .await
            .unwrap()
            .owner,
        Some(active.current_owner)
    );
}
#[tokio::test]
async fn boot_transition_preserves_unacknowledged_distance_and_old_receipt_cannot_restore_boot() {
    let f = setup().await;
    let p = prepare(&f);
    let first = transport::mutate(&f.state, f.node.node_id, &p)
        .await
        .unwrap();
    let a = activate(&f, &first);
    let active = transport::mutate(&f.state, f.node.node_id, &a)
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let new_boot = Uuid::new_v4();
    // Synthetic owner-only fixture replacement, not proof of physical host safety.
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{identity,boot_id}',$2) WHERE node_id=$1").bind(f.node.node_id).bind(json!(new_boot)).execute(&pool).await.unwrap();
    let r = Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: f.node.node_id,
        operation: Operation::AdvanceBoot {
            marker: active.current_owner.marker.clone(),
            expected_boot_id: f.node.boot_id,
            expected_revision: active.current_owner.revision,
            new_boot_id: new_boot,
            next_sequence: 50,
        },
    };
    let advanced = transport::mutate(&f.state, f.node.node_id, &r)
        .await
        .unwrap();
    assert_eq!(advanced.accepted_through, 0);
    assert_eq!(advanced.current_owner.current_boot_id, new_boot);
    let old = transport::mutate(&f.state, f.node.node_id, &a)
        .await
        .unwrap();
    assert_eq!(old.receipt, active.receipt);
    assert_eq!(old.current_owner, advanced.current_owner);
    assert_eq!(rows(&f).await, (1, 3));
    // Returning to an earlier boot UUID is refused even if a synthetic common
    // registration accepts it. Neither receipt history nor pending facts reset.
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{identity,boot_id}',$2) WHERE node_id=$1").bind(f.node.node_id).bind(json!(f.node.boot_id)).execute(&pool).await.unwrap();
    let reused = Request {
        version: VERSION,
        operation_id: Uuid::new_v4(),
        node_id: f.node.node_id,
        operation: Operation::AdvanceBoot {
            marker: advanced.current_owner.marker.clone(),
            expected_boot_id: new_boot,
            expected_revision: advanced.current_owner.revision,
            new_boot_id: f.node.boot_id,
            next_sequence: 50,
        },
    };
    assert!(
        transport::mutate(&f.state, f.node.node_id, &reused)
            .await
            .is_err()
    );
    assert_eq!(rows(&f).await, (1, 3));
    let mut bad = r.clone();
    bad.operation_id = Uuid::new_v4();
    assert!(
        transport::mutate(&f.state, f.node.node_id, &bad)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn foreign_identity_revocation_and_prior_history_refuse_without_allocating() {
    let f = setup().await;
    let p = prepare(&f);
    assert!(
        transport::mutate(&f.state, Uuid::new_v4(), &p)
            .await
            .is_err()
    );
    assert_eq!(rows(&f).await, (0, 0));
    let pool = f.db.pool().await;
    sqlx::query(
        "INSERT INTO observation_nodes(node_id,through_sequence,event_count) VALUES($1,1,1)",
    )
    .bind(f.node.node_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        transport::mutate(&f.state, f.node.node_id, &p)
            .await
            .is_err()
    );
    assert!(transport::status(&f.state, f.node.node_id).await.is_err());
    assert_eq!(rows(&f).await, (0, 0));
    let g = setup().await;
    let q = prepare(&g);
    transport::mutate(&g.state, g.node.node_id, &q)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(g.node.node_id)
        .execute(&g.db.pool().await)
        .await
        .unwrap();
    assert!(
        transport::mutate(&g.state, g.node.node_id, &q)
            .await
            .is_err()
    );
    assert!(transport::status(&g.state, g.node.node_id).await.is_err());
    assert_eq!(rows(&g).await, (1, 1));
}
#[tokio::test]
async fn http_transport_admission_uses_current_node_credentials_and_bounded_private_documents() {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use tower::ServiceExt;
    let f = setup().await;
    let r = prepare(&f);
    let bytes = serde_json::to_vec(&r).unwrap();
    let router = aq_server::build_worker_router(f.state.clone());
    let req = |credential: &str, bytes: Vec<u8>| {
        HttpRequest::builder()
            .method("POST")
            .uri("/v1/worker/observations/transport")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {credential}"))
            .body(Body::from(bytes))
            .unwrap()
    };
    for credential in ["aqn_wrong", "aqa_agent-credential"] {
        assert_eq!(
            router
                .clone()
                .oneshot(req(credential, bytes.clone()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let reply = router
        .clone()
        .oneshot(req(&f.credential, bytes.clone()))
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["cache-control"], "no-store");
    let body = axum::body::to_bytes(reply.into_body(), 16384)
        .await
        .unwrap();
    aq_observation_admission::decode_reply(&body, &r).unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(req(&f.credential, vec![b' '; 16385]))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut foreign = r.clone();
    foreign.node_id = Uuid::new_v4();
    assert_eq!(
        router
            .clone()
            .oneshot(req(&f.credential, serde_json::to_vec(&foreign).unwrap()))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let status = router
        .oneshot(
            HttpRequest::builder()
                .uri("/v1/worker/observations/transport")
                .header("authorization", format!("Bearer {}", f.credential))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let body = axum::body::to_bytes(status.into_body(), 16384)
        .await
        .unwrap();
    aq_observation_admission::decode_status(&body).unwrap();
    assert_eq!(rows(&f).await, (1, 1));
}
#[tokio::test]
async fn stale_authenticated_context_cannot_allocate_or_activate_store() {
    let f = setup().await;
    let r = prepare(&f);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    let context = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    let replacement = aq_server::tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier(&replacement))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(
        transport::mutate_authenticated(&f.state, &context, &r)
            .await
            .is_err()
    );
    assert_eq!(rows(&f).await, (0, 0));
    headers.insert(
        "authorization",
        format!("Bearer {replacement}").parse().unwrap(),
    );
    let current = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    let prepared = transport::mutate_authenticated(&f.state, &current, &r)
        .await
        .unwrap();
    let a = activate(&f, &prepared);
    assert!(
        transport::mutate_authenticated(&f.state, &context, &a)
            .await
            .is_err()
    );
    assert_eq!(rows(&f).await, (1, 1));
    transport::mutate_authenticated(&f.state, &current, &a)
        .await
        .unwrap();
    assert_eq!(rows(&f).await, (1, 2));
}
