use super::{
    AppState, Arc, DefaultBodyLimit, Deserialize, Driver, Extension, Json, Path, RequireOwner,
    Result, Router, Uuid, Value, json, owner, post,
};

/// D9 artifact routes. Artifacts are sent as JSON and re-encoded canonically;
/// the digest check then proves the bytes are the sealed, immutable artifact.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/playbooks/artifacts", post(install_artifact))
        .route("/api/playbooks/artifacts/validate", post(validate_artifact))
        .route(
            "/api/playbooks/bindings/{id}/activate",
            post(activate_binding),
        )
        .route(
            "/api/playbooks/bindings/{id}/deactivate",
            post(deactivate_binding),
        )
        .route(
            "/api/playbooks/bindings/{id}/rollback",
            post(rollback_binding),
        )
        .route("/api/playbooks/coding/default", post(coding_default))
        .layer(DefaultBodyLimit::max(2 * artifacts::MAX_ARTIFACT_BYTES))
}
use crate::playbooks::{
    artifacts::{self, DriverCatalog, PgLedger},
    coding::CodingPolicy,
};
type CatalogAccess = (RequireOwner, Extension<Arc<Driver>>);

fn authorized((RequireOwner(user), Extension(driver)): &CatalogAccess) -> Result<&Driver> {
    owner(driver, user.id)
}

enum CatalogChange<'a> {
    Activate { binding: &'a str, artifact: &'a str },
    Rollback(&'a str),
    CodingDefault(&'a CodingPolicy),
}
impl CatalogChange<'_> {
    async fn apply(
        self,
        access: &CatalogAccess,
        op_id: &str,
        (grant, revision): (Uuid, i64),
    ) -> Result<Json<Value>> {
        let driver = authorized(access)?;
        let catalog = DriverCatalog {
            driver,
            grant,
            revision,
        };
        let ledger = ledger(driver);
        let receipt = match self {
            CatalogChange::Activate { binding, artifact } => {
                artifacts::activate(&catalog, &ledger, op_id, binding, artifact).await
            }
            CatalogChange::Rollback(binding) => {
                artifacts::rollback(&catalog, &ledger, op_id, binding).await
            }
            CatalogChange::CodingDefault(policy) => {
                artifacts::install_coding_default(&catalog, &ledger, op_id, policy).await
            }
        }?;
        Ok(Json(receipt))
    }
}
fn ledger(driver: &Driver) -> PgLedger {
    PgLedger {
        pool: driver.service.pool.clone(),
        namespace_id: driver.service.namespace.namespace_id,
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactBody {
    artifact: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallBody {
    op_id: String,
    artifact: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivateBody {
    op_id: String,
    artifact: String,
    grant_id: Uuid,
    grant_revision: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackBody {
    op_id: String,
    grant_id: Uuid,
    grant_revision: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingDefaultBody {
    op_id: String,
    policy: CodingPolicy,
    grant_id: Uuid,
    grant_revision: i64,
}
async fn validate_artifact(
    access: CatalogAccess,
    Json(r): Json<ArtifactBody>,
) -> Result<Json<Value>> {
    let driver = authorized(&access)?;
    let bytes = artifacts::bytes_of(&r.artifact)?;
    let artifact = artifacts::validate(driver.service.registry(), &bytes)?;
    Ok(Json(artifacts::summary(&artifact)))
}
async fn install_artifact(
    access: CatalogAccess,
    Json(r): Json<InstallBody>,
) -> Result<Json<Value>> {
    let driver = authorized(&access)?;
    let bytes = artifacts::bytes_of(&r.artifact)?;
    // Install needs no activation authority; the grant fields are unused.
    let catalog = DriverCatalog {
        driver,
        grant: Uuid::nil(),
        revision: 0,
    };
    Ok(Json(
        artifacts::install(&catalog, &ledger(driver), &r.op_id, &bytes).await?,
    ))
}
async fn activate_binding(
    access: CatalogAccess,
    Path(id): Path<String>,
    Json(r): Json<ActivateBody>,
) -> Result<Json<Value>> {
    CatalogChange::Activate {
        binding: &id,
        artifact: &r.artifact,
    }
    .apply(&access, &r.op_id, (r.grant_id, r.grant_revision))
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeactivateBody {
    revision: u64,
}
async fn deactivate_binding(
    access: CatalogAccess,
    Path(id): Path<String>,
    Json(r): Json<DeactivateBody>,
) -> Result<Json<Value>> {
    let driver = authorized(&access)?;
    driver.deactivate_binding(id, r.revision).await?;
    Ok(Json(json!({"deactivated": true})))
}
async fn rollback_binding(
    access: CatalogAccess,
    Path(id): Path<String>,
    Json(r): Json<RollbackBody>,
) -> Result<Json<Value>> {
    CatalogChange::Rollback(&id)
        .apply(&access, &r.op_id, (r.grant_id, r.grant_revision))
        .await
}
async fn coding_default(
    access: CatalogAccess,
    Json(r): Json<CodingDefaultBody>,
) -> Result<Json<Value>> {
    CatalogChange::CodingDefault(&r.policy)
        .apply(&access, &r.op_id, (r.grant_id, r.grant_revision))
        .await
}
