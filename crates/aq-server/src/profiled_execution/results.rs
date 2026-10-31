//! Immutable native result claims. Recording never evaluates eligibility,
//! clears a held task, releases source/effects, or changes the graph.
use super::{artifacts, registration};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Kind, Policy},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactKind, Digest, SessionIdentity,
    result_record::{self, Disposition, Receipt},
    transport::{Feature, ResultReport},
};
use sqlx::Row;
use uuid::Uuid;
fn conflict() -> AppError {
    AppError::Conflict("result report conflicts with original stopped owner or report head".into())
}
fn db(error: sqlx::Error) -> AppError {
    if error
        .as_database_error()
        .is_some_and(|e| matches!(e.code().as_deref(), Some("23505" | "23514")))
    {
        conflict()
    } else {
        AppError::Internal
    }
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    report: &ResultReport,
) -> Result<Receipt, AppError> {
    result_record::validate_report(report)
        .map_err(|_| AppError::BadRequest("invalid result report".into()))?;
    let binding = &report.binding;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        return Err(conflict());
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("result node mismatch".into()));
    }
    let bytes = canonical(report)?;
    let digest = result_record::report_digest(report).map_err(|_| conflict())?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, node).await?;
    let admitted = registration::admitted_runtime_locked(&mut tx, node_id, boot_id).await?;
    let view =
        authority::load_locked(state, &mut tx, &binding.session, Policy::IdentityOnly).await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
        || !view.stopped
    {
        return Err(conflict());
    }
    let stop=sqlx::query("SELECT request_bytes,request_digest,receipt_bytes FROM profiled_stopped_receipts WHERE attempt_id=$1 FOR SHARE")
        .bind(binding.attempt_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(conflict)?;
    let stop_bytes: Vec<u8> = stop.get("request_bytes");
    if Digest::of(&stop_bytes).as_str() != stop.get::<String, _>("request_digest") {
        return Err(AppError::Internal);
    }
    let original =
        aq_admission::stopped::decode_request(&stop_bytes).map_err(|_| AppError::Internal)?;
    let stopped =
        aq_admission::stopped::decode_receipt(&stop.get::<Vec<u8>, _>("receipt_bytes"), &original)
            .map_err(|_| AppError::Internal)?;
    if canonical(&report.stop)? != canonical(&original.evidence)?
        || admitted.request.runtime != original.runtime
        || admitted.qualification != original.qualification
        || !admitted
            .request
            .features
            .contains(&Feature::HostStopEvidenceV1)
        || !admitted
            .request
            .features
            .contains(&Feature::DurableHostJournalV1)
        || !matches!(view.stop.as_ref(),Some(crate::execution::profiled::Stop::Stopped {receipt}) if *receipt==stopped.artifact().map_err(|_|AppError::Internal)?)
    {
        return Err(conflict());
    }
    let started_bytes: Vec<u8> = sqlx::query_scalar(
        "SELECT request_bytes FROM profiled_started_receipts WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let started =
        aq_admission::started::decode_request(&started_bytes).map_err(|_| AppError::Internal)?;
    original
        .validate_after(&started)
        .map_err(|_| AppError::Internal)?;
    let launch = sqlx::query(
        "SELECT execution_spec,launch_manifest FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let spec = aq_admission::execution_spec::decode_profiled_spec(&canonical(
        &launch.get::<serde_json::Value, _>("execution_spec"),
    )?)
    .map_err(|_| conflict())?;
    let manifest: aq_session_protocol::LaunchManifest =
        serde_json::from_value(launch.get("launch_manifest")).map_err(|_| AppError::Internal)?;
    if spec.outcome_artifact != manifest.bindings.outcome {
        return Err(conflict());
    }
    let artifact_id:Uuid=sqlx::query_scalar("SELECT a.artifact_id FROM admission_artifacts a JOIN profiled_launch_artifacts l USING(artifact_id) WHERE l.attempt_id=$1 AND a.owner_kind='outcome' AND a.owner_key=$2 AND a.descriptor->'artifact'=$3 ORDER BY a.artifact_id LIMIT 1")
        .bind(binding.attempt_id).bind(view.project_id.to_string()).bind(serde_json::to_value(&spec.outcome_artifact).map_err(|_|AppError::Internal)?)
        .fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(conflict)?;
    let outcome = artifacts::load_locked(&mut tx, artifact_id).await?;
    if outcome.owner != artifacts::OwnerKind::Outcome
        || outcome.project_id != view.project_id
        || outcome.descriptor.kind != ArtifactKind::Outcome
        || outcome.descriptor.artifact != spec.outcome_artifact
    {
        return Err(conflict());
    }
    let definition = aq_admission::outcome::decode(&outcome.bytes).map_err(|_| conflict())?;
    definition.validate_against(&spec).map_err(|_| conflict())?;
    let existing=sqlx::query("SELECT report_id,attempt_id,node_id,operation_id,report_bytes,report_digest,receipt_bytes FROM profiled_result_reports WHERE report_id=$1 OR(node_id=$2 AND operation_id=$3)")
        .bind(report.report_id).bind(node_id).bind(report.operation_id).fetch_all(&mut *tx).await.map_err(db)?;
    if !existing.is_empty() {
        if existing.len() != 1 {
            return Err(conflict());
        }
        let old = &existing[0];
        if old.get::<Uuid, _>("report_id") != report.report_id
            || old.get::<Uuid, _>("attempt_id") != binding.attempt_id
            || old.get::<Uuid, _>("node_id") != node_id
            || old.get::<Uuid, _>("operation_id") != report.operation_id
            || old.get::<Vec<u8>, _>("report_bytes") != bytes
            || old.get::<String, _>("report_digest") != digest.as_str()
        {
            return Err(conflict());
        }
        let receipt =
            result_record::decode_receipt(&old.get::<Vec<u8>, _>("receipt_bytes"), report)
                .map_err(|_| AppError::Internal)?;
        if receipt.expected_outcome != spec.outcome_artifact
            || receipt.stopped_receipt != stopped.artifact().map_err(|_| AppError::Internal)?
        {
            return Err(AppError::Internal);
        }
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    if !view.holds_task {
        return Err(conflict());
    }
    let head: Option<Uuid> = sqlx::query_scalar(
        "SELECT report_id FROM profiled_result_heads WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if report.supersedes != head {
        return Err(conflict());
    }
    let (count,used):(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(octet_length(report_bytes)),0)::BIGINT FROM profiled_result_reports WHERE attempt_id=$1")
        .bind(binding.attempt_id).fetch_one(&mut *tx).await.map_err(db)?;
    if count >= 64
        || used
            .checked_add(bytes.len() as i64)
            .is_none_or(|n| n > 4_194_304)
    {
        return Err(AppError::PayloadTooLarge(
            "result report retention quota exhausted".into(),
        ));
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| conflict())?;
    let receipt = Receipt {
        schema_version: 1,
        operation_id: report.operation_id,
        report_id: report.report_id,
        binding: binding.clone(),
        report_digest: digest.clone(),
        expected_outcome: spec.outcome_artifact,
        stopped_receipt: stopped.artifact().map_err(|_| AppError::Internal)?,
        recorded_at_ms: now,
        disposition: Disposition::Recorded,
    };
    receipt.validate_for(report).map_err(|_| conflict())?;
    sqlx::query("INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,supersedes,report_bytes,report_digest,receipt_bytes,outcome_artifact_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(report.report_id).bind(binding.attempt_id).bind(node_id).bind(report.operation_id).bind(report.supersedes).bind(&bytes).bind(digest.as_str()).bind(canonical(&receipt)?).bind(artifact_id).execute(&mut *tx).await.map_err(db)?;
    sqlx::query(if head.is_some() {
        "UPDATE profiled_result_heads SET report_id=$2 WHERE attempt_id=$1"
    } else {
        "INSERT INTO profiled_result_heads(attempt_id,report_id) VALUES($1,$2)"
    })
    .bind(binding.attempt_id)
    .bind(report.report_id)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/worker/profiled/results", axum::routing::post(report))
        .layer(axum::extract::DefaultBodyLimit::max(
            result_record::MAX_BYTES,
        ))
        .with_state(state)
}
async fn report(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<ResultReport>,
) -> Result<axum::Json<Receipt>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(axum::Json(record(&state, &node, &request).await?))
}
