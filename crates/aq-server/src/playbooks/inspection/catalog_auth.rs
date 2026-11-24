//! Original HTTP principal pinned across the new current-catalog owner wait.
//! Kept separate from legacy UUID-only inspection APIs.
use super::{Error, Service};
use crate::playbooks::inspection::catalog::CatalogReadError;
use crate::store::{Role, User};
use sqlx::Row;
impl Service {
    pub(crate) async fn authorize_current_catalog(
        &self,
        user: &User,
    ) -> std::result::Result<String, CatalogReadError> {
        if user.id != self.namespace.owner_user_id {
            return Err(Error::Forbidden.into());
        }
        if !has_owner_identity(user) {
            return Err(CatalogReadError::Access);
        }
        let mut tx = self.tx().await?;
        let row=sqlx::query("SELECT role,email,issuer,subject,xmin::text AS version FROM users WHERE id=$1 FOR SHARE").bind(user.id).fetch_optional(&mut *tx).await?.ok_or(CatalogReadError::Access)?;
        if !matches_identity(&row, user) {
            return Err(CatalogReadError::Access);
        }
        self.namespace_locked(&mut tx).await?;
        let version = row.get("version");
        tx.commit().await?;
        Ok(version)
    }
}

fn has_owner_identity(user: &User) -> bool {
    user.role == Role::Owner
        && [user.issuer.as_deref(), user.subject.as_deref()]
            .into_iter()
            .all(|field| field.is_some_and(|id| !id.is_empty()))
}
fn matches_identity(row: &sqlx::postgres::PgRow, user: &User) -> bool {
    let role: &str = row.get("role");
    let email: &str = row.get("email");
    let issuer: Option<&str> = row.get("issuer");
    let subject: Option<&str> = row.get("subject");
    (role, email, issuer, subject)
        == (
            "owner",
            user.email.as_str(),
            user.issuer.as_deref(),
            user.subject.as_deref(),
        )
}
