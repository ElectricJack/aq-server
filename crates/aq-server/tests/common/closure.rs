//! Actual mounted offer/acceptance owners; the runtime/installation claims are
//! synthetic qualification fixtures, never a physical model/harness claim.
use super::native_offer::{self, Case};
use aq_admission::{
    ArtifactRef, Digest, MaterialReceipt, Text,
    control::InitialGrantDelivery,
    startup::NativeOffer,
    transport::{ArtifactReceipt, PreparationPlan, Prepare},
};
use aq_server::{
    profiled_execution::initial_grant,
    worker_api::{self, AuthenticatedNode},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;
pub struct Prepared {
    pub case: Case,
    pub offer: NativeOffer,
    pub plan: PreparationPlan,
    pub request: Prepare,
    pub clock: Arc<AtomicI64>,
}
pub async fn auth(c: &Prepared) -> AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.case.env.env.credential)
            .parse()
            .unwrap(),
    );
    worker_api::authenticate_context(&c.case.env.env.state, &headers)
        .await
        .unwrap()
}
pub async fn setup() -> Prepared {
    let (case, offer) = native_offer::published().await;
    setup_from(case, offer).await
}
async fn setup_from(mut case: Case, offer: NativeOffer) -> Prepared {
    let accepted = native_offer::acceptance(&case, &offer);
    let plan = native_offer::accept(&case, &accepted).await.original;
    let clock = Arc::new(AtomicI64::new(plan.clock.server_time_ms as i64 + 1));
    let at = clock.clone();
    // The case clock is frozen, as in `native_offer::prepare_request`. The
    // offer's facts are valid for a few seconds of that frozen time, so a clock
    // that also followed the wall clock expired them whenever the fixture took
    // longer than that (a loaded 4-core runner) and the grant was refused as
    // "selection expired during admission locks".
    case.env.env.state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(at.load(Ordering::SeqCst)).unwrap()
    });
    let artifacts = artifact_receipts(&case, &offer, &plan).await;
    let materials = material_receipts(&plan);
    let request = Prepare {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: accepted.identity,
        binding: plan.binding.clone(),
        reservation: plan.reservation.clone(),
        guardian_id: plan.guardian_id,
        launch_id: plan.launch_id,
        journal_revision: accepted.journal_revision + 1,
        journal_digest: Digest::of(b"synthetic durable Prepared intent"),
        artifacts,
        materials,
    };
    aq_admission::transport::TransportDocument::Prepare(request.clone())
        .validate()
        .unwrap();
    Prepared {
        case,
        offer,
        plan,
        request,
        clock,
    }
}
async fn artifact_receipts(
    case: &Case,
    offer: &NativeOffer,
    plan: &PreparationPlan,
) -> Vec<ArtifactReceipt> {
    let mut artifacts = offer
        .offered
        .artifacts
        .iter()
        .map(|a| ArtifactReceipt {
            artifact: a.artifact.clone(),
            bytes: a.bytes,
            digest: a.artifact.digest.clone(),
        })
        .collect::<Vec<_>>();
    let transfer: Value =
        sqlx::query_scalar("SELECT manifest FROM source_bases WHERE attempt_id=$1")
            .bind(plan.binding.attempt_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    let transfer: aq_worker_protocol::TransferManifest = serde_json::from_value(transfer).unwrap();
    for (name, hash, bytes) in [
        (
            "manifest",
            transfer.manifest_digest.as_str(),
            transfer.manifest_bytes,
        ),
        (
            "bundle",
            transfer.bundle_digest.as_deref().unwrap(),
            transfer.bundle_bytes,
        ),
    ] {
        let digest: Digest = serde_json::from_value(json!(hash)).unwrap();
        artifacts.push(ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!("aq.source-{name}/{}", transfer.transfer_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: digest.clone(),
            },
            bytes,
            digest,
        });
    }
    artifacts
}

fn material_receipts(plan: &PreparationPlan) -> Vec<MaterialReceipt> {
    plan.materials
        .iter()
        .map(|l| MaterialReceipt {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            lease_id: l.lease_id,
            binding: l.binding.clone(),
            material_digest: l.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!("aq.private-material/{}", l.lease_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(
                    b"synthetic opaque installer record: no physical worker qualification",
                ),
            },
            received_at_ms: plan.clock.server_time_ms,
        })
        .collect()
}

pub async fn grant(c: &Prepared) -> Result<InitialGrantDelivery, aq_server::error::AppError> {
    initial_grant::prepare(
        &c.case.env.env.state,
        &auth(c).await,
        &c.request,
        Uuid::new_v4(),
    )
    .await
}
