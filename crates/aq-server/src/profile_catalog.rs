//! Central, revision-checked activation of immutable validated profile artifacts.
//! Pure catalog/compiler semantics live in aq-profiles. No provider is contacted.
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_profiles::{ActivationState, ValidatedCatalog};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
mod inspection;

const MAX_ARTIFACT_BYTES: usize = 2 * aq_profiles::MAX_CATALOG_BYTES;
const MAX_CATALOGS: i64 = 128;
const MAX_RETAINED_BYTES: i64 = 64 * 1024 * 1024;
const MAX_REGISTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_OPERATIONS: i64 = 100_000;
type Result<T> = std::result::Result<T, AppError>;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Activate {
    operation_id: Uuid,
    expected_revision: u64,
    /// Exact exported JSON artifact, revalidated before activation. This is not
    /// a local filesystem path or an instruction to fetch arbitrary network data.
    artifact_json: String,
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .merge(inspection::routes())
        .route("/api/profiles/catalogs", get(inspect))
        .route("/api/profiles/catalogs/{digest}", get(artifact))
        .route("/api/profiles/catalogs/activate", post(activate))
        // A JSON string can expand each source byte into six escaped bytes.
        .layer(DefaultBodyLimit::max(6 * MAX_ARTIFACT_BYTES + 4096))
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn invalid() -> AppError {
    AppError::BadRequest("invalid profile catalog artifact".into())
}
fn conflict(reason: &str) -> AppError {
    AppError::Conflict(reason.into())
}
fn profile(error: aq_profiles::Error) -> AppError {
    match error {
        aq_profiles::Error::RevisionConflict => conflict("catalog revision changed"),
        aq_profiles::Error::IdentityConflict(_) => conflict("immutable profile identity changed"),
        _ => invalid(),
    }
}
fn digest_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
async fn inspect(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Value>> {
    let row =
        sqlx::query("SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_optional(&state.store.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("profile catalog registry has not been installed"))?;
    Ok(Json(
        json!({"revision":row.get::<i64,_>("revision"),"active_digest":row.get::<Option<String>,_>("active_digest")}),
    ))
}
async fn artifact(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(digest): Path<String>,
) -> Result<Json<Value>> {
    if !digest_valid(&digest) {
        return Err(invalid());
    }
    let bytes: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(&digest)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(db)?;
    let bytes = bytes.ok_or_else(|| AppError::NotFound("catalog artifact not found".into()))?;
    let catalog = tokio::task::spawn_blocking(move || ValidatedCatalog::from_artifact(&bytes))
        .await
        .map_err(|_| AppError::Internal)?
        .map_err(|_| AppError::Internal)?;
    if catalog.digest() != digest {
        return Err(AppError::Internal);
    }
    Ok(Json(json!({"digest":digest,"artifact":catalog.artifact()})))
}
async fn activate(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<Activate>,
) -> Result<Json<Value>> {
    if request.operation_id.is_nil() || request.expected_revision >= i64::MAX as u64 {
        return Err(invalid());
    }
    if request.artifact_json.len() > MAX_ARTIFACT_BYTES {
        return Err(AppError::PayloadTooLarge(
            "catalog artifact exceeds 2 MiB".into(),
        ));
    }
    let operation_id = request.operation_id;
    let expected_revision = request.expected_revision;
    // Canonical request identity includes the artifact's exact exported string;
    // changing even its whitespace under a previously used operation ID conflicts.
    let payload_digest = aq_profiles::digest(&request).map_err(profile)?;
    let catalog = tokio::task::spawn_blocking(move || {
        ValidatedCatalog::from_artifact(request.artifact_json.as_bytes())
    })
    .await
    .map_err(|_| AppError::Internal)?
    .map_err(profile)?;
    let bytes = catalog.canonical_bytes().map_err(profile)?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(invalid());
    }
    // C4: every environment pins exactly one allowlisted template digest.
    let templates = state
        .config
        .templates
        .as_deref()
        .map(|allowlist| {
            crate::templates::resolve(crate::templates::catalog_environments(&catalog), allowlist)
                .map_err(conflict)
        })
        .transpose()?;

    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // Serialize activation, identity tombstones, quota and operation receipts in
    // one owning database transaction, including across daemon processes.
    let row = sqlx::query(
        "SELECT revision,state FROM profile_catalog_registry WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let old = sqlx::query("SELECT payload_digest,response FROM profile_catalog_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(user.id).bind(operation_id).fetch_optional(&mut *tx).await.map_err(db)?;
    if let Some(old) = old {
        if old.get::<String, _>("payload_digest") != payload_digest {
            return Err(conflict("operation ID reused with changed content"));
        }
        let response = old.get("response");
        tx.commit().await.map_err(db)?;
        return Ok(Json(response));
    }
    let revision: i64 = row.get("revision");
    let current: ActivationState =
        serde_json::from_value(row.get("state")).map_err(|_| AppError::Internal)?;
    if current.revision != revision as u64 {
        return Err(AppError::Internal);
    }
    let next = current
        .prepare_activation(expected_revision, &catalog)
        .map_err(profile)?;
    let next_bytes = serde_json::to_vec(&next).map_err(|_| AppError::Internal)?;
    if next_bytes.len() > MAX_REGISTRY_BYTES {
        return Err(conflict("catalog identity retention quota reached"));
    }
    let old_bytes: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(catalog.digest())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
    if let Some(old_bytes) = old_bytes {
        if old_bytes != bytes {
            return Err(conflict("catalog digest collision"));
        }
    } else {
        let retained = sqlx::query("SELECT count(*) AS count,coalesce(sum(octet_length(artifact)),0)::bigint AS bytes FROM profile_catalog_artifacts")
            .fetch_one(&mut *tx).await.map_err(db)?;
        if retained.get::<i64, _>("count") >= MAX_CATALOGS
            || retained.get::<i64, _>("bytes") + bytes.len() as i64 > MAX_RETAINED_BYTES
        {
            return Err(conflict(
                "retained catalog quota reached; existing artifacts preserved",
            ));
        }
        sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,$4)")
            .bind(catalog.digest()).bind(bytes).bind(user.id).bind((state.clock)().timestamp_millis())
            .execute(&mut *tx).await.map_err(db)?;
    }
    if let Some(templates) = &templates {
        crate::templates::store(&mut tx, catalog.digest(), templates).await?;
    }
    let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_catalog_operations")
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if operations >= MAX_OPERATIONS {
        return Err(conflict("catalog operation retention quota reached"));
    }
    let next_value = serde_json::to_value(&next).map_err(|_| AppError::Internal)?;
    sqlx::query(
        "UPDATE profile_catalog_registry SET revision=$1,active_digest=$2,state=$3 WHERE singleton",
    )
    .bind(next.revision as i64)
    .bind(catalog.digest())
    .bind(next_value)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let mut response = json!({"operation_id":operation_id,"revision":next.revision,"active_digest":catalog.digest(),"disposition":"activated"});
    if let Some(templates) = templates {
        response["environment_templates"] = json!(templates);
    }
    sqlx::query("INSERT INTO profile_catalog_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(user.id).bind(operation_id).bind(payload_digest).bind(&response).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(Json(response))
}
