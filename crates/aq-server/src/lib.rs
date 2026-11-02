pub mod admin_api;
pub mod admission;
pub mod agent_access;
pub mod authz;
pub mod config;
pub mod dashboard_api;
pub mod dashboard_assets;
pub mod error;
pub mod execution;
pub mod identity;
pub mod intelligence;
pub mod notifications;
pub mod observations;
pub mod playbooks;
pub mod profile_catalog;
pub mod profiled_execution;
pub mod provider_accounts;
pub mod project_bindings;
pub mod protected_storage;
pub mod readiness;
pub mod result_media;
pub mod reviews;
pub mod session_access;
pub mod source_retention;
pub mod store;
pub mod terminal;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tokens;
pub mod updates;
pub mod worker_api;
use crate::config::Config;
use crate::identity::IdentityVerifier;
use crate::store::Store;
use aq_protocol::{SUPPORTED_WORKER_PROTOCOLS, VersionInfo};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::{HeaderValue, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::IntoResponse;
use axum::response::Response;
use axum::{Json, Router, routing::get};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;

/// The database state format: always the newest migration's number, because
/// every migration is a state-format change (`store` tests enforce it). The
/// updater restores the pre-update backup on rollback only when the format
/// changes, so a migration without a bump would strand the old release.
pub const STATE_FORMAT_VERSION: u32 = 40;
/// State formats this binary can read; always contains `STATE_FORMAT_VERSION`.
pub const READABLE_STATE_FORMATS: &[u32] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
];
/// Versions that may be upgraded to this release.
pub const UPGRADE_FROM: &str = ">=0.1.0";

pub fn version_info() -> VersionInfo {
    VersionInfo {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        source_revision: option_env!("AQ_SOURCE_REVISION")
            .unwrap_or("dev")
            .to_string(),
        protocol_min: SUPPORTED_WORKER_PROTOCOLS.0,
        protocol_max: SUPPORTED_WORKER_PROTOCOLS.1,
        state_format_version: STATE_FORMAT_VERSION,
    }
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Source of "now". Every expiry and offline computation reads this so tests
/// can use a fixed clock.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

#[derive(Clone)]
pub struct AppState {
    pub agent_access: Option<Arc<agent_access::Runtime>>,
    pub source_retention: Option<Arc<source_retention::Runtime>>,
    pub config: Arc<Config>,
    pub store: Store,
    pub identity: Arc<dyn IdentityVerifier>,
    pub clock: Clock,
}

impl AppState {
    pub(crate) fn terminal_read_view(&self) -> Self {
        Self {
            store: self.store.terminal_read_view(),
            ..self.clone()
        }
    }
    pub(crate) fn terminal_view(&self) -> Self {
        Self {
            store: self.store.terminal_view(),
            ..self.clone()
        }
    }
    pub(crate) fn presence_view(&self) -> Self {
        Self {
            store: self.store.presence_view(),
            ..self.clone()
        }
    }
    pub(crate) fn renewal_view(&self) -> Self {
        Self {
            store: self.store.renewal_view(),
            ..self.clone()
        }
    }
    pub(crate) fn control_view(&self) -> Self {
        Self {
            store: self.store.control_view(),
            ..self.clone()
        }
    }
}

async fn harden(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) != Some("private, no-store") {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

async fn not_found() -> Response {
    error::AppError::NotFound("no such route".into()).into_response()
}

async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(aq_protocol::ApiError {
            error: "method_not_allowed".into(),
            message: "method not allowed for this route".into(),
        }),
    )
        .into_response()
}

/// Human routes plus `/healthz`. Worker routes are not mounted here.
///
/// `csrf_guard` is layered over the whole merged router (fallbacks included),
/// so it covers every admin route regardless of declaration order. A POST to
/// an unmatched path therefore gets 403 `csrf_rejected` unless it carries
/// the CSRF header.
pub fn build_admin_router(state: AppState) -> Router {
    build_admin_router_with_playbooks(state, None)
}
/// Explicit optional runtime injection. Default construction mounts no playbook
/// routes; injected routes share existing owner auth/CSRF/maintenance layers.
pub fn build_admin_router_with_playbooks(
    state: AppState,
    playbooks: Option<Arc<playbooks::driver::Driver>>,
) -> Router {
    build_admin_router_with_policy_owners(state, playbooks, None)
}
/// Explicit configured owner injection; default constructors mount no review
/// endpoints. All injected owners share existing auth/CSRF/maintenance layers.
pub fn build_admin_router_with_policy_owners(
    state: AppState,
    playbooks: Option<Arc<playbooks::driver::Driver>>,
    reviews: Option<Arc<reviews::Service>>,
) -> Router {
    build_admin_router_with_optional_dashboard(state, playbooks, reviews, None)
}
/// Explicit test/embedding injection. The machine router never receives assets.
pub fn build_admin_router_with_dashboard(
    state: AppState,
    dashboard: Arc<dashboard_assets::DashboardAssets>,
) -> Router {
    build_admin_router_with_optional_dashboard(state, None, None, Some(dashboard))
}
/// Explicit embedding of configured policy owners and a verified static bundle.
/// Uses the same human authorization, capability, CSRF and maintenance layers.
pub fn build_admin_router_with_dashboard_and_policy_owners(
    state: AppState,
    playbooks: Option<Arc<playbooks::driver::Driver>>,
    reviews: Option<Arc<reviews::Service>>,
    dashboard: Arc<dashboard_assets::DashboardAssets>,
) -> Router {
    build_admin_router_with_optional_dashboard(state, playbooks, reviews, Some(dashboard))
}
fn build_admin_router_with_optional_dashboard(
    state: AppState,
    playbooks: Option<Arc<playbooks::driver::Driver>>,
    reviews: Option<Arc<reviews::Service>>,
    dashboard: Option<Arc<dashboard_assets::DashboardAssets>>,
) -> Router {
    build_admin_router_with_optional_owners(state, playbooks, reviews, dashboard, None)
}
/// Explicit configured media injection alongside the existing human owners.
/// Default constructors continue to mount no media endpoints.
pub fn build_admin_router_with_optional_owners(
    state: AppState,
    playbooks: Option<Arc<playbooks::driver::Driver>>,
    reviews: Option<Arc<reviews::Service>>,
    dashboard: Option<Arc<dashboard_assets::DashboardAssets>>,
    media: Option<Arc<result_media::runtime::Runtime>>,
) -> Router {
    let mounted = dashboard_api::MountedOwners {
        graph: true,
        session_usage: true,
        session_events: true,
        results: true,
        media_read: media.as_ref().is_some_and(|r| r.read_enabled()),
        media_upload: media.as_ref().is_some_and(|r| r.uploads_enabled()),
        notifications: true,
        supervisor_conversation: true,
        reviews: reviews.is_some(),
        playbook_loaded_definitions: playbooks
            .as_ref()
            .is_some_and(|driver| driver.loaded_definitions_available()),
        playbook_catalog: playbooks.as_ref().map(|driver| driver.catalog_support()),
        playbook_owner: playbooks
            .as_ref()
            .map(|d| d.service.namespace.owner_user_id),
    };
    let policy_routes = playbooks.map(playbooks::api::routes).unwrap_or_default();
    let review_routes = reviews.map(reviews::admin_routes).unwrap_or_default();
    let media_routes = media.map(result_media::api::routes).unwrap_or_default();
    let app = Router::new()
        .merge(
            dashboard
                .as_ref()
                .map(|a| dashboard_assets::routes(a.clone()))
                .unwrap_or_default(),
        )
        .merge(policy_routes)
        .merge(review_routes)
        .merge(media_routes)
        .merge(admin_api::admin_routes())
        .merge(dashboard_api::routes(state.clone()))
        .merge(agent_access::human::routes(&state))
        .merge(agent_access::supervisor_conversation::routes(&state))
        .merge(profiled_execution::human_results::routes())
        .merge(provider_accounts::routes())
        .merge(notifications::api::routes(state.clone()))
        .merge(observations::api::admin_routes(state.clone()))
        .merge(terminal::api::human_routes(state.clone()))
        .merge(profile_catalog::admin_routes())
        .merge(project_bindings::admin_routes())
        .merge(intelligence::admin_routes())
        .merge(agent_access::admin_routes(state.clone()))
        .merge(source_retention::admin_routes())
        .merge(execution::api::admin_routes(state.clone()))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readiness::readyz))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(from_fn_with_state(
            state.clone(),
            updates::admin_maintenance_gate,
        ))
        .layer(from_fn_with_state(state.clone(), authz::csrf_guard))
        .layer(from_fn_with_state(
            state.store.admission.clone(),
            admission::gate,
        ))
        .layer(from_fn(harden))
        .layer(axum::Extension(mounted));
    let app = if let Some(dashboard) = dashboard {
        app.layer(axum::Extension(dashboard))
    } else {
        app
    };
    app.with_state(state)
}

/// Machine routes plus `/healthz`. No human routes are mounted here.
pub fn build_worker_router(state: AppState) -> Router {
    build_worker_router_inner(state, None)
}
/// Candidate recovery owner is absent unless a trusted caller explicitly
/// installs and injects a historical verifier policy. This is not activation
/// of any worker proof producer or of settlement/release.
pub fn build_worker_router_with_closure_policy(
    state: AppState,
    policy: Arc<profiled_execution::closure::policy::InstalledPolicy>,
) -> Router {
    build_worker_router_inner(state, Some(policy))
}
fn build_worker_router_inner(
    state: AppState,
    policy: Option<Arc<profiled_execution::closure::policy::InstalledPolicy>>,
) -> Router {
    let closure = match policy {
        Some(policy) => profiled_execution::closure::api::routes(state.clone(), policy),
        None => Router::new(),
    };
    Router::new()
        .merge(closure)
        .merge(worker_api::worker_routes(state.clone()))
        .merge(profiled_execution::api::routes(state.clone()))
        .merge(profiled_execution::control_api::routes(state.clone()))
        .merge(profiled_execution::accepted_preparation::routes(
            state.clone(),
        ))
        .merge(profiled_execution::phase_execution::transport::routes(
            state.clone(),
        ))
        .merge(profiled_execution::reconciliation::routes(state.clone()))
        .merge(profiled_execution::renewal::routes(state.clone()))
        .merge(profiled_execution::presence::routes(state.clone()))
        .merge(profiled_execution::producer::api::routes(state.clone()))
        .merge(profiled_execution::phase_terminal::routes::routes(
            state.clone(),
        ))
        .merge(profiled_execution::started::routes(state.clone()))
        .merge(profiled_execution::stopped::routes(state.clone()))
        .merge(profiled_execution::results::routes(state.clone()))
        .merge(profiled_execution::material_api::routes(state.clone()))
        .merge(profiled_execution::artifact_api::routes(state.clone()))
        .merge(profiled_execution::source_api::routes(state.clone()))
        .merge(observations::api::routes(state.clone()))
        .merge(observations::api::transport_routes(state.clone()))
        .merge(intelligence::worker_routes())
        .merge(agent_access::machine_routes(state.clone()))
        .merge(source_retention::machine_routes(state.clone()))
        .merge(execution::api::worker_routes(state.clone()))
        .merge(terminal::api::worker_routes(state.clone()))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readiness::readyz))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(from_fn_with_state(
            state.clone(),
            updates::worker_maintenance_gate,
        ))
        .layer(from_fn_with_state(
            state.store.admission.clone(),
            admission::gate,
        ))
        .layer(from_fn(harden))
        .with_state(state)
}

/// Serves both routers until `shutdown` resolves, then drains gracefully.
pub async fn run_with_listeners(
    state: AppState,
    admin: tokio::net::TcpListener,
    worker: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    run_with_listeners_and_dashboard(state, admin, worker, shutdown, None).await
}
async fn run_with_listeners_and_dashboard(
    state: AppState,
    admin: tokio::net::TcpListener,
    worker: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    dashboard: Option<Arc<dashboard_assets::DashboardAssets>>,
) -> anyhow::Result<()> {
    profiled_execution::policies::install(&state)
        .await
        .map_err(|_| anyhow::anyhow!("profiled policy installation failed"))?;
    if state.config.terminals.is_some() {
        terminal::persistence::install(&state)
            .await
            .map_err(|_| anyhow::anyhow!("terminal runtime installation failed"))?;
    }
    let notification_dispatcher = uuid::Uuid::new_v4();
    notifications::api::service(&state)
        .install(notification_dispatcher)
        .await?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let media_runtime = match &state.config.result_media {
        Some(settings) => {
            Some(result_media::runtime::Runtime::start(state.clone(), settings).await?)
        }
        None => None,
    };
    let mut policy_runtime = match state.config.playbooks.as_ref() {
        Some(config) => match playbooks::runtime::Runtime::start(&state, config.clone()).await {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                if let Some(runtime) = &media_runtime
                    && runtime.shutdown().await.is_err()
                {
                    return Err(error.context("media cleanup also failed during startup"));
                }
                return Err(error);
            }
        },
        None => None,
    };
    if let Some(runtime) = &mut policy_runtime {
        runtime.watch_shutdown(rx.clone());
    }

    let notification_task = tokio::spawn(notifications::runtime::run(
        state.clone(),
        notification_dispatcher,
        rx.clone(),
    ));
    let terminal_task = tokio::spawn(terminal::runtime::run(state.clone(), rx.clone()));
    let expiry_task = tokio::spawn(profiled_execution::expiry::run(state.clone(), rx.clone()));
    tokio::spawn(async move {
        shutdown.await;
        let _ = tx.send(true);
    });
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let admin_app = build_admin_router_with_optional_owners(
        state.clone(),
        policy_runtime.as_ref().map(|r| r.driver.clone()),
        policy_runtime.as_ref().map(|r| r.reviews.clone()),
        dashboard,
        media_runtime.clone(),
    );
    let worker_app = build_worker_router(state);
    let serving = tokio::try_join!(
        async {
            axum::serve(
                admin,
                admin_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(wait(rx.clone()))
            .await
        },
        async {
            axum::serve(
                worker,
                worker_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(wait(rx.clone()))
            .await
        },
    );
    terminal_task.abort();
    let _ = terminal_task.await;
    notification_task.abort();
    let _ = notification_task.await;
    expiry_task.abort();
    let _ = expiry_task.await;
    let policy_shutdown = match policy_runtime {
        Some(runtime) => runtime.shutdown().await,
        None => Ok(()),
    };
    let media_shutdown = match media_runtime {
        Some(runtime) => runtime.shutdown().await.map_err(anyhow::Error::from),
        None => Ok(()),
    };
    serving?;
    policy_shutdown?;
    media_shutdown?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received");
}

/// Binds both listeners and serves until SIGTERM or ctrl-c.
pub async fn run(config: Config, store: Store) -> anyhow::Result<()> {
    let dashboard = std::env::var_os("AQ_DASHBOARD_DIR")
        .map(|path| {
            dashboard_assets::DashboardAssets::load(std::path::Path::new(&path)).map(Arc::new)
        })
        .transpose()?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let admin = tokio::net::TcpListener::bind(config.server.admin_bind).await?;
    let worker = tokio::net::TcpListener::bind(config.server.worker_bind).await?;
    tracing::info!(admin = %admin.local_addr()?, worker = %worker.local_addr()?, "listening");
    let agent_access = match &config.agent_access {
        Some(settings) => Some(Arc::new(agent_access::Runtime::new(settings).await?)),
        None => None,
    };
    let source_retention = match &config.source_retention {
        Some(settings) => Some(Arc::new(source_retention::Runtime::new(settings.clone())?)),
        None => None,
    };
    let state = AppState {
        agent_access,
        source_retention,
        identity: identity::verifier_from_config(&config.auth, http),
        config: Arc::new(config),
        store,
        clock: system_clock(),
    };
    run_with_listeners_and_dashboard(state, admin, worker, shutdown_signal(), dashboard).await
}
