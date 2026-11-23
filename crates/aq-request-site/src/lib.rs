//! The method and matched route template of a request, for the API refusal log.
//!
//! [`record`] is a middleware that keeps the request's method and matched route
//! template (`/api/human/conversations/{conversation}/messages`) for the rest of
//! the request; [`warn_refused`] logs a refusal with them. Never the raw path or
//! the query string, which can carry ids and cursors.

use axum::extract::{MatchedPath, Request};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

#[derive(Clone, Debug)]
struct RequestSite {
    method: Method,
    route: String,
}

tokio::task_local! {
    static REQUEST_SITE: RequestSite;
}

/// Middleware: make the request's method and matched route template known to
/// [`warn_refused`] for the rest of the request. A request that matched no route
/// is logged as `(unmatched)`.
pub async fn record(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "(unmatched)".to_owned(), |p| p.as_str().to_owned());
    let site = RequestSite {
        method: req.method().clone(),
        route,
    };
    REQUEST_SITE.scope(site, next.run(req)).await
}

/// Log the "API request refused" warning with the current request's method and
/// route template (`(unknown)` outside [`record`]). `message` is logged only for
/// a conflict (409): other refusals' text may repeat the caller's input.
pub fn warn_refused(status: u16, code: &str, message: &str) {
    let message = if status == 409 { message } else { "" };
    let (method, route) = REQUEST_SITE
        .try_with(|site| (site.method.to_string(), site.route.clone()))
        .unwrap_or_else(|_| ("(unknown)".into(), "(unknown)".into()));
    tracing::warn!(%method, %route, status, %code, message, "API request refused");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::{Router, middleware::from_fn, routing::get};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl MakeWriter<'_> for Sink {
        type Writer = Sink;
        fn make_writer(&self) -> Sink {
            self.clone()
        }
    }

    async fn refused(app: Router, uri: &str) -> String {
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = axum::http::Request::get(uri).body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    async fn not_found() -> Response {
        warn_refused(404, "not_found", "no thing 6f1c2b0e");
        StatusCode::NOT_FOUND.into_response()
    }

    #[tokio::test]
    async fn a_refusal_log_names_the_method_and_route_template_but_not_the_query() {
        let app = Router::new()
            .route("/things/{thing}/messages", get(not_found))
            .layer(from_fn(record));
        let id = "6f1c2b0e-7d58-4f0a-9a43-0c2f5a1e9b77";
        let log = refused(
            app,
            &format!("/things/{id}/messages?before=41.2&token=sekrit"),
        )
        .await;
        assert!(log.contains("API request refused"), "{log}");
        assert!(log.contains("method=GET"), "{log}");
        assert!(log.contains("route=/things/{thing}/messages"), "{log}");
        assert!(
            log.contains("status=404") && log.contains("code=not_found"),
            "{log}"
        );
        for leaked in [id, "before", "41.2", "token", "sekrit", "?"] {
            assert!(!log.contains(leaked), "{leaked} in {log}");
        }
    }

    #[tokio::test]
    async fn a_refusal_without_a_matched_route_says_so() {
        let app = Router::new().fallback(not_found).layer(from_fn(record));
        let log = refused(app, "/nowhere/1234?cursor=9").await;
        assert!(log.contains("route=(unmatched)"), "{log}");
        assert!(!log.contains("nowhere") && !log.contains("cursor"), "{log}");
    }
}
