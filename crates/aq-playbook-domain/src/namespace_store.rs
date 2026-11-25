//! Persistent metadata identifying a playbook namespace and its catalog generation.
pub mod startup;

use crate::{Error, Namespace, Result, digest, review_domain::CatalogProfile};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

pub struct NamespaceRecord<'a> {
    scope: &'a Namespace,
    profile: CatalogProfile,
    source: &'a str,
}
impl<'a> NamespaceRecord<'a> {
    pub fn new(scope: &'a Namespace, profile: CatalogProfile, source: &'a str) -> Self {
        Self {
            scope,
            profile,
            source,
        }
    }
    pub async fn lock(&self, db: &mut PgConnection) -> Result<PgRow> {
        let row = sqlx::query(
            "SELECT * FROM playbook_namespaces WHERE namespace_id=$1 AND generation=$2 FOR UPDATE",
        )
        .bind(self.scope.namespace_id)
        .bind(self.scope.generation)
        .fetch_optional(db)
        .await?
        .ok_or(Error::Forbidden)?;
        if row.try_get::<i16, _>("catalog_profile").unwrap_or(1) != self.profile.number() {
            return Err(Error::Conflict);
        }
        if !self.same_scope(&row) {
            return Err(Error::Conflict);
        }
        if row.get::<String, _>("catalog_digest") != digest(&self.profile.registry())?
            || row.get::<String, _>("source_digest") != self.source
        {
            return Err(Error::Conflict);
        }
        Ok(row)
    }
    fn same_scope(&self, row: &PgRow) -> bool {
        let stored = (
            row.get::<Uuid, _>("owner_user_id"),
            row.get::<Uuid, _>("workspace_id"),
            row.get::<Uuid, _>("project_id"),
        );
        let expected = (
            self.scope.owner_user_id,
            self.scope.workspace_id,
            self.scope.project_id,
        );
        stored == expected && row.get::<String, _>("name") == self.scope.name
    }
    pub async fn initialize(&self, db: &mut PgConnection, actor: Uuid) -> Result<()> {
        if self.profile == CatalogProfile::Mail1 {
            sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace_id) DO NOTHING").bind(self.scope.namespace_id).bind(self.scope.generation).bind(&self.scope.name).bind(self.scope.workspace_id).bind(self.scope.project_id).bind(actor).bind(digest(&self.profile.registry())?).bind(self.source).execute(db).await?;
        } else {
            sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest,catalog_profile) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(namespace_id) DO NOTHING").bind(self.scope.namespace_id).bind(self.scope.generation).bind(&self.scope.name).bind(self.scope.workspace_id).bind(self.scope.project_id).bind(actor).bind(digest(&self.profile.registry())?).bind(self.source).bind(self.profile.number()).execute(db).await?;
        }
        Ok(())
    }
}
