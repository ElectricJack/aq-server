//! Playbook authoring (spec 0013): the server side of `aq-playbook-authoring`.
//! The crate does the work; this says where the engine, the grants, internal
//! work and the clock are.
use crate::playbooks::{domain::GrantScope, driver::Driver};
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_playbook_authoring::{self as authoring, Error, Result, SourceKind, SourceRecord};
use serde_json::Value;
use sqlx::types::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// The engine authored playbooks are installed in: empty until it is open.
#[derive(Clone, Default)]
pub struct Deployment(Arc<RwLock<Option<Arc<Driver>>>>);

impl Deployment {
    pub(crate) fn set(&self, driver: Arc<Driver>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = Some(driver);
        }
    }
    fn driver(&self) -> Result<Arc<Driver>> {
        let driver = self.0.read().ok().and_then(|slot| slot.clone());
        driver.ok_or_else(|| Error::Conflict("Playbooks are not running on this server".into()))
    }
}

fn engine(e: crate::playbooks::domain::Error) -> Error {
    Error::Conflict(format!("The playbook engine refused: {e}"))
}

type Grant = (Uuid, i64, Json<BTreeSet<String>>, Option<i64>);
async fn grant_row(state: &AppState, source: &str) -> Result<Option<Grant>> {
    Ok(sqlx::query_as(
        "SELECT grant_id,revision,capabilities,revoked_at_ms FROM playbook_source_grants WHERE source_id=$1",
    )
    .bind(source)
    .fetch_optional(&state.store.pool)
    .await?)
}

fn scope(driver: &Driver, capabilities: &BTreeSet<String>) -> GrantScope {
    let namespace = &driver.service.namespace;
    GrantScope {
        workspace_id: namespace.workspace_id,
        project_id: namespace.project_id,
        recipients: vec![],
        capabilities: capabilities.clone(),
    }
}

impl authoring::Host for AppState {
    async fn markdown(&self, source: &SourceRecord) -> Result<(String, u64)> {
        let stored = match source.kind {
            SourceKind::Registered => {
                authoring::stored_markdown(&self.store.pool, &source.id).await?
            }
            // The routing document is part of the Intelligence structure.
            SourceKind::Builtin if source.id == authoring::builtin::ROUTING => {
                let read = aq_profile_settings::structure::document(&self.store.pool).await;
                let (document, revision, _) = read.map_err(|e| Error::Host(e.to_string()))?;
                Some((document.routing_markdown, revision))
            }
            SourceKind::Builtin => None,
        };
        stored.ok_or_else(|| Error::Host(format!("{} has no document yet", source.id)))
    }
    async fn facts(&self, source: &SourceRecord) -> Result<BTreeMap<String, Value>> {
        let mut facts = BTreeMap::new();
        for name in &source.facts {
            if name != authoring::builtin::ROUTING_OPTIONS {
                return Err(Error::Host(format!("there is no fact provider {name}")));
            }
            // What Settings offers: the facts `levels`, `offers`, `harnesses`, `providers`.
            let options = crate::routing::options(self).await;
            facts.extend(options.map_err(|e| Error::Host(format!("{e:?}")))?.facts());
        }
        Ok(facts)
    }
}

impl authoring::Deployer for AppState {
    async fn install(&self, artifact: &authoring::model::Artifact) -> Result<()> {
        let driver = self.internal_work.authoring().driver()?;
        driver.install(artifact.clone()).await.map_err(engine)
    }
    async fn ensure_grant(
        &self,
        source: &str,
        capabilities: &BTreeSet<String>,
    ) -> Result<(Uuid, i64)> {
        let driver = self.internal_work.authoring().driver()?;
        let (service, pool) = (&driver.service, &self.store.pool);
        let owner = service.namespace.owner_user_id;
        let mut replaced = None;
        if let Some((id, revision, held, revoked)) = grant_row(self, source).await? {
            // Live in this namespace generation, with these abilities: reuse.
            let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM playbook_grants WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3 AND revoked_at_ms IS NULL)")
                .bind(id).bind(service.namespace.namespace_id).bind(service.namespace.generation)
                .fetch_one(pool).await?;
            if live && revoked.is_none() && held.0 == *capabilities {
                return Ok((id, revision));
            }
            replaced = live.then_some(id);
        }
        let id = Uuid::new_v4();
        (service.source_grant(id, scope(&driver, capabilities)).await).map_err(engine)?;
        sqlx::query("INSERT INTO playbook_source_grants(source_id,grant_id,revision,capabilities,created_at_ms) VALUES($1,$2,1,$3,$4) ON CONFLICT(source_id) DO UPDATE SET grant_id=EXCLUDED.grant_id,revision=1,capabilities=EXCLUDED.capabilities,created_at_ms=EXCLUDED.created_at_ms,revoked_at_ms=NULL")
            .bind(source).bind(id).bind(Json(capabilities)).bind(authoring::Server::now_ms(self))
            .execute(pool).await?;
        if let Some(old) = replaced {
            // What runs now moves to the new grant before the old one goes, so
            // a playbook in effect keeps working while its successor is
            // written. When the new abilities no longer cover it, it stops.
            let binding: Option<String> =
                sqlx::query_scalar("SELECT binding FROM playbook_sources WHERE id=$1")
                    .bind(source)
                    .fetch_optional(pool)
                    .await?;
            let current = match &binding {
                Some(binding) => driver
                    .binding_activation(binding.clone())
                    .await
                    .map_err(engine)?,
                None => None,
            };
            if let (Some(binding), Some(current)) = (binding, current)
                && let Err(error) = driver.activate(binding, current.artifact, id, 1).await
            {
                tracing::warn!(
                    source,
                    ?error,
                    "the playbook in effect is not allowed by the new grant"
                );
            }
            service.revoke(owner, old).await.map_err(engine)?;
        }
        Ok((id, 1))
    }
    async fn revoke_grant(&self, source: &str) -> Result<()> {
        let Some((id, _, _, None)) = grant_row(self, source).await? else {
            return Ok(());
        };
        let driver = self.internal_work.authoring().driver()?;
        let owner = driver.service.namespace.owner_user_id;
        driver.service.revoke(owner, id).await.map_err(engine)?;
        sqlx::query("UPDATE playbook_source_grants SET revoked_at_ms=$2 WHERE source_id=$1")
            .bind(source)
            .bind(authoring::Server::now_ms(self))
            .execute(&self.store.pool)
            .await?;
        Ok(())
    }
    async fn activate(&self, source: &str, binding: &str, digest: &str) -> Result<()> {
        let driver = self.internal_work.authoring().driver()?;
        let Some((grant, revision, held, None)) = grant_row(self, source).await? else {
            return Err(Error::Conflict(format!("{source} has no grant")));
        };
        let authority =
            (driver.service.profile).authority(grant, revision, &scope(&driver, &held.0));
        let current = driver
            .binding_activation(binding.into())
            .await
            .map_err(engine)?;
        if current
            .is_some_and(|a| a.artifact == digest && Some(&a.authority) == authority.as_ref().ok())
        {
            return Ok(());
        }
        let activated = driver
            .activate(binding.into(), digest.into(), grant, revision)
            .await;
        activated.map(drop).map_err(engine)
    }
}

impl authoring::Server for AppState {
    fn config(&self) -> authoring::Config {
        self.config.playbook_authoring.clone()
    }
    fn now_ms(&self) -> i64 {
        (self.clock)().timestamp_millis()
    }
    async fn submit(&self, id: Uuid, request: &aq_internal_work::Request) -> Result<()> {
        let submitted = self.internal_work.submit_as(id, request).await;
        submitted
            .map(drop)
            .map_err(authoring::complete::internal_error)
    }
}

impl From<Error> for AppError {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(m) => Self::BadRequest(m),
            Error::Conflict(m) | Error::Host(m) => Self::Conflict(m),
            Error::NotFound(m) => Self::NotFound(m),
            Error::Database(e) => crate::error::db(e),
            Error::Corrupt => Self::Internal,
        }
    }
}

pub(crate) fn admin_routes() -> axum::Router<AppState> {
    authoring::http::routes::<AppState, RequireOwner, AppError>()
}
