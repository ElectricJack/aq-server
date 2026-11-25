use super::{AppState, Arc, Duration, Loaded, Runtime, Slot, watch};

/// Serving when the owner could not open at startup: the server runs with the
/// playbook routes answering "playbooks unavailable: <reason>" (503), and the
/// open is retried with a backoff (the drive interval, doubling up to the
/// engine statement timeout). Once it opens, the router `build` makes with
/// the owner replaces the unavailable one and the owner goes into `slot`.
pub(crate) struct DeferredMount {
    pub(crate) slot: Slot,
    pub(crate) stop: watch::Receiver<bool>,
}
impl DeferredMount {
    pub(crate) fn open_later(
        self,
        state: AppState,
        (config, failure): (Arc<Loaded>, anyhow::Error),
        build: impl Fn(Option<&Runtime>) -> axum::Router + Send + Sync + 'static,
    ) -> axum::Router {
        let Self { slot, stop } = self;
        use axum::response::IntoResponse;
        let reason = Arc::new(std::sync::RwLock::new(failure.to_string()));
        let shown = reason.clone();
        let unavailable = move |_: crate::authz::RequireOwner| {
            let reason = shown.read().map(|r| r.clone()).unwrap_or_default();
            let message = format!("playbooks unavailable: {reason}");
            let body = serde_json::json!({"error":"playbooks_unavailable","message":message});
            async move {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(body),
                )
            }
        };
        let fallback = axum::Router::new()
            .route("/api/playbooks/{*rest}", axum::routing::any(unavailable))
            .with_state(state.clone());
        let current = Arc::new(std::sync::RwLock::new(build(None).merge(fallback)));
        let serving = current.clone();
        tokio::spawn(async move {
            let backoff =
                aq_playbook_host::drive::Backoff::new(config.interval, config.timeouts.statement());
            let attempt = || Runtime::start(&state, config.clone());
            let failed = |error: &anyhow::Error, delay: Duration| {
                tracing::warn!(%error, ?delay, "playbook owner unavailable; retrying the open");
                if let Ok(mut reason) = reason.write() {
                    *reason = error.to_string();
                }
            };
            let opened =
                aq_playbook_host::drive::retry(stop.clone(), backoff, attempt, failed).await;
            if let Some(mut runtime) = opened {
                if *stop.borrow() {
                    // Opened after shutdown began: nobody will take it from the slot.
                    return drop(runtime.shutdown().await);
                }
                runtime.watch_shutdown(stop);
                if let Ok(mut current) = current.write() {
                    *current = build(Some(&runtime));
                }
                *slot.lock().await = Some(runtime);
            }
        });
        let route = move |request: axum::extract::Request| {
            let router = serving.read().map(|r| r.clone()).ok();
            async move {
                match router {
                    Some(router) => tower::ServiceExt::oneshot(router, request)
                        .await
                        .into_response(),
                    None => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
        };
        axum::Router::new().fallback(route)
    }
}
