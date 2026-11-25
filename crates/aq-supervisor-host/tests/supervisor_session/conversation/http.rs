use super::*;
#[derive(Clone, Copy)]
pub(super) struct Client<'a> {
    pub(super) app: &'a axum::Router,
    pub(super) headers: &'a [(&'a str, &'a str)],
}
impl Client<'_> {
    pub(super) async fn call(
        self,
        method: &str,
        path: &str,
        body: Option<String>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let Self { app, headers } = self;
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let mut r = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        let response = app
            .clone()
            .oneshot(
                r.body(axum::body::Body::from(body.unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| serde_json::json!({"text": String::from_utf8_lossy(&bytes)})),
        )
    }
}
pub(super) fn conversation_schema(v: &serde_json::Value) {
    let s: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}
/// Pages the whole conversation forward and backward with small limits. Every
/// item (owner or supervisor) has a distinct cursor, every page respects the
/// limit, and the pages concatenate to `expected` with nothing lost or repeated.
pub(super) async fn assert_history_pages_exactly(
    app: &axum::Router,
    messages: &str,
    headers: &[(&str, &str)],
    expected: &[Uuid],
) {
    let pages = Pages {
        client: Client { app, headers },
        messages,
    };
    for limit in [1usize, 5, 16] {
        let seen = pages.forward(expected, limit).await;
        pages.backward(expected, limit).await;
        // A reply cursor resumes exactly between two replies of the same turn.
        let middle = &seen[expected.len() / 2];
        let tail = pages.get(format!("after={}&limit=16", middle.1)).await;
        assert_eq!(
            ids(&tail).first().map(|s| s.0),
            Some(expected[expected.len() / 2 + 1])
        );
    }
}
fn ids(page: &serde_json::Value) -> Vec<(Uuid, String)> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["message_id"].as_str().unwrap().parse().unwrap(),
                i["cursor"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}
struct Pages<'a> {
    client: Client<'a>,
    messages: &'a str,
}
impl Pages<'_> {
    async fn get(&self, query: String) -> serde_json::Value {
        let (c, page) = self
            .client
            .call("GET", &format!("{}?{query}", self.messages), None)
            .await;
        assert_eq!(c, axum::http::StatusCode::OK, "{query}: {page}");
        conversation_schema(&page);
        page
    }
    async fn forward(&self, expected: &[Uuid], limit: usize) -> Vec<(Uuid, String)> {
        // Forward from the start.
        let mut seen: Vec<(Uuid, String)> = vec![];
        let mut after = "0".to_owned();
        loop {
            let page = self.get(format!("after={after}&limit={limit}")).await;
            let items = ids(&page);
            assert!(items.len() <= limit && !items.is_empty(), "{page}");
            seen.extend(items);
            if !page["has_more"].as_bool().unwrap() {
                assert!(page["next_cursor"].is_null(), "{page}");
                break;
            }
            after = page["next_cursor"].as_str().unwrap().to_owned();
            assert_eq!(
                &after,
                &seen.last().unwrap().1,
                "next_cursor is the last item"
            );
        }
        assert_eq!(
            seen.iter().map(|s| s.0).collect::<Vec<_>>(),
            expected,
            "forward limit {limit}"
        );
        let cursors: std::collections::HashSet<&String> = seen.iter().map(|s| &s.1).collect();
        assert_eq!(cursors.len(), expected.len(), "cursors are distinct");
        seen
    }
    async fn backward(&self, expected: &[Uuid], limit: usize) {
        // Backward from the newest.
        let mut pages: Vec<Vec<(Uuid, String)>> = vec![];
        let mut query = format!("limit={limit}");
        loop {
            let page = self.get(query).await;
            let items = ids(&page);
            assert!(items.len() <= limit && !items.is_empty(), "{page}");
            let more = page["has_more"].as_bool().unwrap();
            let next = page["next_cursor"].as_str().map(str::to_owned);
            if more {
                assert_eq!(
                    next.as_ref(),
                    Some(&items[0].1),
                    "next_cursor is the oldest item"
                );
            }
            pages.push(items);
            let Some(next) = next.filter(|_| more) else {
                break;
            };
            query = format!("before={next}&limit={limit}");
        }
        let backward: Vec<Uuid> = pages.into_iter().rev().flatten().map(|s| s.0).collect();
        assert_eq!(backward, expected, "backward limit {limit}");
    }
}
