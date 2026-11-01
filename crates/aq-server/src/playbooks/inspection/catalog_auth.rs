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
        if user.role != Role::Owner
            || user.issuer.as_ref().is_none_or(|value| value.is_empty())
            || user.subject.as_ref().is_none_or(|value| value.is_empty())
        {
            return Err(CatalogReadError::Access);
        }
        let mut tx = self.tx().await?;
        let row=sqlx::query("SELECT role,email,issuer,subject,xmin::text AS version FROM users WHERE id=$1 FOR SHARE").bind(user.id).fetch_optional(&mut *tx).await?.ok_or(CatalogReadError::Access)?;
        if row.get::<String, _>("role") != "owner"
            || row.get::<String, _>("email") != user.email
            || row.get::<Option<String>, _>("issuer") != user.issuer
            || row.get::<Option<String>, _>("subject") != user.subject
        {
            return Err(CatalogReadError::Access);
        }
        self.namespace_locked(&mut tx).await?;
        let version = row.get("version");
        tx.commit().await?;
        Ok(version)
    }
}
