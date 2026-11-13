//! Internal work (spec 0012): the server side of `aq-internal-work`.
use crate::{AppState, Clock, authz::RequireOwner, error::AppError};
use aq_intelligence_config::LoadedRegistry;
use aq_internal_work::{Error, InternalWorkConfig, Outcome, Request, http, service::Service};
use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// The service callers use (`submit`, `wait`, `run`) and the in-server loop.
/// `Default` (tests only) is not wired: every call fails and nothing runs.
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(Default))]
pub struct InternalWork(Option<Arc<Service>>);

impl InternalWork {
    pub fn new(
        pool: PgPool,
        clock: Clock,
        config: Option<&InternalWorkConfig>,
        registry: Option<Arc<LoadedRegistry>>,
    ) -> aq_internal_work::Result<Self> {
        let now = Arc::new(move || clock().timestamp_millis());
        let service = Service::new(pool, now, config, registry)?;
        Ok(Self(Some(Arc::new(service))))
    }
    fn service(&self) -> Result<&Service, AppError> {
        let unwired = || AppError::Conflict("internal work is not available".into());
        self.0.as_deref().ok_or_else(unwired)
    }
    pub async fn submit(&self, request: Request) -> Result<Uuid, AppError> {
        Ok(self.service()?.submit(&request).await?)
    }
    pub async fn wait(&self, id: Uuid) -> Result<Outcome, AppError> {
        Ok(self.service()?.wait(id).await?)
    }
    pub async fn run(&self, request: Request) -> Result<Outcome, AppError> {
        Ok(self.service()?.run(&request).await?)
    }
    /// Runs the loop (`Service::serve`, which calls `Service::tick`) until `stop`.
    pub async fn serve(self, stop: tokio::sync::watch::Receiver<bool>) {
        if let Some(service) = self.0 {
            service.serve(stop).await;
        }
    }
}

impl AsRef<PgPool> for AppState {
    fn as_ref(&self) -> &PgPool {
        &self.store.pool
    }
}
impl From<RequireOwner> for Uuid {
    fn from(owner: RequireOwner) -> Uuid {
        owner.0.id
    }
}
impl From<Error> for AppError {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(m) => Self::BadRequest(m),
            Error::Conflict(m) => Self::Conflict(m),
            Error::NotFound(m) => Self::NotFound(m),
            Error::Database(e) => crate::error::db(e),
            Error::Corrupt => Self::Internal,
        }
    }
}
pub(crate) fn admin_routes() -> Router<AppState> {
    http::routes::<AppState, RequireOwner, AppError>()
}
