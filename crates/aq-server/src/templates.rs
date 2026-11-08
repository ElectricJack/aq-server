//! C4 environment templates; moved to the aq-environment-templates crate.
use crate::error::AppError;
pub use aq_environment_templates::{
    Allowlist, Resolution, catalog_environments, deserialize, resolve,
};

pub async fn store(
    tx: &mut sqlx::PgConnection,
    catalog_digest: &str,
    resolutions: &[Resolution],
) -> Result<(), AppError> {
    aq_environment_templates::store(tx, catalog_digest, resolutions)
        .await
        .map_err(|_| AppError::Internal)
}

pub async fn load(
    conn: &mut sqlx::PgConnection,
    catalog_digest: &str,
) -> Result<Vec<Resolution>, AppError> {
    aq_environment_templates::load(conn, catalog_digest)
        .await
        .map_err(|_| AppError::Internal)
}
