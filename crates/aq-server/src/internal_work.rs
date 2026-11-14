//! Internal work (spec 0012): the server side of `aq-internal-work`.
use crate::agent_access::supervisor_conversation::system_message;
use crate::{AppState, Clock, authz::RequireOwner, error::AppError};
use aq_intelligence_config::LoadedRegistry;
use aq_internal_work::service::{NotifySupervisor, Service, settings_choices};
use aq_internal_work::{Error, InternalWorkConfig, Outcome, Request, http};
use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// The service callers use (`submit`, `wait`, `run`) and the in-server loop.
/// `Default` (tests only) is not wired: every call fails and nothing runs.
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(Default))]
pub struct InternalWork(Option<Arc<Service>>, crate::playbook_authoring::Deployment);

impl InternalWork {
    pub fn new(
        pool: PgPool,
        clock: Clock,
        config: Option<&InternalWorkConfig>,
        registry: Option<Arc<LoadedRegistry>>,
        preferences: Option<std::path::PathBuf>,
    ) -> aq_internal_work::Result<Self> {
        let now: aq_internal_work::service::Now = Arc::new(move || clock().timestamp_millis());
        let (notify_pool, notify_now) = (pool.clone(), now.clone());
        let notify: NotifySupervisor = Arc::new(move |project, body| {
            let (pool, at_ms) = (notify_pool.clone(), notify_now() as u64);
            Box::pin(async move {
                let sent = system_message(&pool, project, &body, at_ms).await;
                sent.map(drop).map_err(|e| format!("{e:?}"))
            })
        });
        let choices = settings_choices(registry.clone(), preferences);
        let service = Service::new(pool, now, config, registry, choices)?.notify_supervisor(notify);
        Ok(Self(Some(Arc::new(service)), Default::default()))
    }
    /// Where authored playbooks are installed, once the engine is open.
    pub(crate) fn authoring(&self) -> &crate::playbook_authoring::Deployment {
        &self.1
    }
    /// Tests: the engine authored playbooks are installed in.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_engine(&self, driver: Arc<crate::playbooks::driver::Driver>) {
        self.1.set(driver);
    }
    /// Submits under the caller's id; an id already stored makes no second task.
    pub(crate) async fn submit_as(&self, id: Uuid, request: &Request) -> Result<Uuid, Error> {
        let unwired = || Error::Conflict("internal work is not available".into());
        self.0
            .as_deref()
            .ok_or_else(unwired)?
            .submit_as(id, request)
            .await
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
    /// Runs the loop (`Service::serve`: the sweep and the API attempts) until `stop`.
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
impl http::Host for AppState {
    fn service(&self) -> aq_internal_work::Result<&Service> {
        let unwired = || Error::Conflict("internal work is not available".into());
        self.internal_work.0.as_deref().ok_or_else(unwired)
    }
    /// The supervisor admin nest's gate leaves the approving owner on the request.
    fn by_supervisor(extensions: &axum::http::Extensions) -> bool {
        extensions
            .get::<aq_supervisor_control::read::ReadOwner>()
            .is_some()
    }
}
pub(crate) fn admin_routes() -> Router<AppState> {
    http::routes::<AppState, RequireOwner, AppError>()
}
