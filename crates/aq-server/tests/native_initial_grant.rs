//! Initial grant admission. Missing original publication must never be filled in
//! from an acceptance digest or a synthetic compatibility offer.
mod common;
use aq_admission::{ArtifactRef, Digest, MaterialReceipt, Text, transport::Prepare};
use aq_server::profiled_execution::{initial_grant, preparation};
use common::native_preparation::{context, setup};
use uuid::Uuid;
#[tokio::test]
async fn actual_accepted_plan_without_original_native_publication_cannot_issue_a_grant() {
    let c = setup().await;
    let auth = context(&c).await;
    let plan = preparation::accept(&c.f.state, &auth, &c.request, Uuid::new_v4())
        .await
        .unwrap()
        .original;
    let lease = &plan.materials[0];
    let request = Prepare {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: plan.binding.clone(),
        reservation: plan.reservation.clone(),
        guardian_id: plan.guardian_id,
        launch_id: plan.launch_id,
        journal_revision: c.request.journal_revision + 1,
        journal_digest: Digest::of(b"synthetic prepared claim; never admitted"),
        artifacts: vec![],
        materials: vec![MaterialReceipt {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            lease_id: lease.lease_id,
            binding: lease.binding.clone(),
            material_digest: lease.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!("aq.private-material/{}", lease.lease_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"synthetic opaque claim; not physical installation"),
            },
            received_at_ms: plan.clock.server_time_ms,
        }],
    };
    aq_admission::transport::TransportDocument::Prepare(request.clone())
        .validate()
        .unwrap();
    assert!(
        initial_grant::prepare(&c.f.state, &auth, &request, Uuid::new_v4())
            .await
            .is_err()
    );
    let pool = c.f.db.pool().await;
    let hot: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "preparing");
    assert!(hot["grant"].is_null());
    assert!(hot["execution_deadline_ms"].is_null());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT phase FROM profiled_owner_journals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "reserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}
#[test]
fn prepare_uses_reserved_control_capacity() {
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/poll"),
        aq_server::admission::Class::Control
    );
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/prepare"),
        aq_server::admission::Class::Control
    );
}

#[tokio::test]
async fn accept_quota_charges_incoming_jsonb_expansion_at_the_same_retained_byte_boundary() {
    let a = setup().await;
    let plan = preparation::accept(&a.f.state, &context(&a).await, &a.request, Uuid::new_v4())
        .await
        .unwrap()
        .original;
    let pool = a.f.db.pool().await;
    let expanded:i64=sqlx::query_scalar("SELECT (SELECT sum(octet_length(lease::text)) FROM admission_material_leases)+(SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts)").fetch_one(&pool).await.unwrap();
    let compact = serde_json::to_vec(&plan.materials[0]).unwrap().len()
        + serde_json::to_vec(&aq_admission::transport::TransportDocument::PreparationPlan(plan))
            .unwrap()
            .len();
    assert!(expanded > compact as i64 + 20);
    // Identical finite fixture shapes use UUID/digest fields of fixed width.
    // Leave enough for compact encoding, but insufficient for actual JSONB text.
    let gap = compact as i64 + (expanded - compact as i64) / 2;
    drop(pool);
    // The shared selection fixture owns a process-environment gate.
    drop(a);
    let c = setup().await;
    let pool = c.f.db.pool().await;
    let overhead: i32 =
        sqlx::query_scalar("SELECT octet_length(jsonb_build_object('pad','')::text)")
            .fetch_one(&pool)
            .await
            .unwrap();
    let target = 4_194_304 - gap;
    let mut remaining = target;
    while remaining > 0 {
        let charge = remaining.min(200_000);
        assert!(charge > i64::from(overhead));
        sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,jsonb_build_object('pad',repeat('x',$4)))").bind(c.request.identity.node_id).bind(Uuid::new_v4()).bind("a".repeat(64)).bind((charge-i64::from(overhead)) as i32).execute(&pool).await.unwrap();
        remaining -= charge;
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sum(octet_length(response::text)) FROM profiled_execution_receipts"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        target
    );
    assert!(
        preparation::accept(&c.f.state, &context(&c).await, &c.request, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let hot: serde_json::Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "offered");
}

#[path = "native_initial_grant/published.rs"]
mod published;
