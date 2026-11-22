//! The `environment_versions` rows the catalog reads (migration 0164).
use crate::Admitted;
use aq_admission::Digest;
use serde_json::{Value, json};
use sqlx::PgPool;

/// One admitted environment version and the definition body it reads as.
pub(crate) struct Version {
    pub admitted: Admitted,
    /// `sha256:<hex>`, as the builder reported it.
    pub template: String,
    pub image: String,
    pub platform_text: Option<String>,
    pub harnesses: Value,
    pub warm: Value,
    pub body: String,
    pub body_digest: Digest,
}
type Row = (
    String,
    String,
    String,
    String,
    Value,
    Option<String>,
    Value,
    Value,
    Value,
);

/// Every admitted version, newest first. Rolling records bound the rows.
pub(crate) async fn versions(pool: &PgPool) -> sqlx::Result<Vec<Version>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT environment_id,version,template_digest,image,provides,platform_text,toolchain,harnesses,warm \
         FROM environment_versions ORDER BY admitted_at DESC,version DESC",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(version).collect()
}
pub(crate) fn version(row: Row) -> sqlx::Result<Version> {
    let (id, version, template, image, provides, platform_text, toolchain, harnesses, warm) = row;
    let hex = template.strip_prefix("sha256:").unwrap_or_default();
    let digest: Digest = serde_json::from_value(json!(hex))
        .map_err(|e| sqlx::Error::Decode(format!("template digest: {e}").into()))?;
    let body = json!({
        "schema_version": 1,
        "template": template,
        "image": image,
        "provides": provides,
        "platform_text": platform_text,
        "toolchain": toolchain,
    })
    .to_string();
    Ok(Version {
        admitted: Admitted {
            environment: id,
            version,
            digest,
            image: Some(image.clone()),
        },
        body_digest: Digest::of(body.as_bytes()),
        template,
        image,
        platform_text,
        harnesses,
        warm,
        body,
    })
}
