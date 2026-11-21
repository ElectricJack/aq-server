//! A reviewing attempt's read of its project's retained results, where each
//! result's work is kept, and the exact retained patch (`result.read` at
//! project scope). Reads only: it never approves, settles or recreates work,
//! and shows no other project.
// An error here is the finished HTTP response, as in `grant_check`.
#![allow(clippy::result_large_err)]
use super::{Error as AgentError, authenticate, grant_check, id as field, ok};
use crate::{AppState, error::AppError, profiled_execution::human_results};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;

const LIST: &str = "/v1/agent/results/list";
const SHOW: &str = "/v1/agent/results/show";
const PATCH: &str = "/v1/agent/results/patch";
static BUDGET: Semaphore = Semaphore::const_new(2);
/// One patch at a time: each holds up to 4 MiB raw and its base64 text.
static PATCH_BUDGET: Semaphore = Semaphore::const_new(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    api_version: u32,
    after: Option<Uuid>,
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Show {
    api_version: u32,
    report_id: Uuid,
    revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchQuery {
    api_version: u32,
    report_id: Uuid,
    revision: String,
    /// Raw patch bytes the caller will accept: 1..=4 MiB, default the maximum.
    max_bytes: Option<u64>,
}

pub(crate) fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/agent/results/list", post(list))
        .route("/v1/agent/results/show", post(show))
        .route("/v1/agent/results/patch", post(patch))
}
/// The attempt's project, once the credential holds `result.read` for it. The
/// project comes from the binding and never from the body.
async fn authorize(s: &AppState, h: &HeaderMap, path: &str) -> Result<Uuid, Response> {
    let p = authenticate(s, h)
        .await
        .map_err(IntoResponse::into_response)?;
    grant_check::require(s, h, &p, path, None).await?;
    field(&p.binding, "project_id").map_err(IntoResponse::into_response)
}
fn parse<T: for<'de> Deserialize<'de>>(
    body: &[u8],
    version: impl Fn(&T) -> u32,
) -> Result<T, Response> {
    let v: T =
        serde_json::from_slice(body).map_err(|_| AgentError("invalid_request").into_response())?;
    if version(&v) != 1 {
        return Err(AgentError("unsupported_api_version").into_response());
    }
    Ok(v)
}
fn refuse(error: AppError) -> Response {
    AgentError(match error {
        AppError::NotFound(_) => "not_found",
        AppError::BadRequest(_) => "invalid_request",
        AppError::Busy | AppError::IdentityUnavailable(_) | AppError::Maintenance(_) => {
            "overloaded"
        }
        _ => "internal",
    })
    .into_response()
}
/// The bounded read: two at a time and four seconds, as the human routes.
async fn bounded<T>(
    read: impl std::future::Future<Output = Result<T, AppError>>,
) -> Result<T, Response> {
    bounded_by(&BUDGET, 4, read).await
}
async fn bounded_by<T>(
    budget: &Semaphore,
    seconds: u64,
    read: impl std::future::Future<Output = Result<T, AppError>>,
) -> Result<T, Response> {
    let _permit = budget
        .try_acquire()
        .map_err(|_| AgentError("overloaded").into_response())?;
    tokio::time::timeout(std::time::Duration::from_secs(seconds), read)
        .await
        .map_err(|_| AgentError("overloaded").into_response())?
        .map_err(refuse)
}
async fn list(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let project = match authorize(&s, &h, LIST).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match parse::<List>(&body, |q| q.api_version) {
        Ok(q) => q,
        Err(r) => return r,
    };
    match bounded(human_results::read_project(
        &s, project, q.after, q.limit, None,
    ))
    .await
    {
        Ok(page) => ok(json!({
            "api_version": 1, "project_id": project, "coverage": page.coverage,
            "complete": !page.incomplete, "unavailable": page.unavailable,
            "items": page.items, "next_after": page.next_after,
        })),
        Err(r) => r,
    }
}
/// One exact revision of the attempt's project, with what the database
/// recorded of its retained work. A report of another project, a revision
/// that is not the report's current digest and an unknown report are all
/// `NotFound`.
struct Retained {
    coverage: String,
    item: human_results::ResultMetadata,
    recorded: aq_delivery::result_work::Recorded,
}
impl Retained {
    async fn read(
        s: &AppState,
        project: Uuid,
        report: Uuid,
        revision: &str,
    ) -> Result<Self, AppError> {
        let page =
            human_results::read_project(s, project, None, None, Some((report, revision))).await?;
        let item = page
            .items
            .into_iter()
            .next()
            .ok_or(AppError::NotFound("result revision not found".into()))?;
        let recorded = aq_delivery::result_work::recorded(&s.store.pool, item.report_id)
            .await
            .map_err(|_| AppError::Internal)?;
        Ok(Self {
            coverage: page.coverage,
            item,
            recorded,
        })
    }
    fn work(&self) -> Value {
        aq_delivery::result_work::describe(
            &aq_delivery::result_work::Subject {
                report_id: self.item.report_id,
                revision: &self.item.revision,
                attempt_id: self.item.attempt_id,
                source_repository_id: self.item.repository_id,
                source_base: self.item.source_base.hex.as_str(),
                source_receipt_ids: &self.item.source_receipt_ids,
            },
            &self.recorded,
        )
    }
}
async fn show(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let project = match authorize(&s, &h, SHOW).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match parse::<Show>(&body, |q| q.api_version) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let read = async {
        let retained = Retained::read(&s, project, q.report_id, &q.revision).await?;
        let work = retained.work();
        Ok::<Value, AppError>(json!({
            "api_version": 1, "project_id": project, "coverage": retained.coverage,
            "complete": true, "result": retained.item, "work": work,
        }))
    };
    match bounded(read).await {
        Ok(v) => ok(v),
        Err(r) => r,
    }
}

/// `patch.served` is false with a `reason` whenever no verified bytes are
/// returned. Retained work that is absent is reported as evidence, not
/// recreated or approved.
fn unserved(reason: &str) -> Value {
    json!({"served": false, "reason": reason})
}
/// The retained patch of one exact revision: the diff from the base to the
/// head AQ recorded for it, read from aq-git's `diff.patch` and checked
/// (format, both OIDs, size, digest) before any byte is returned.
async fn retained_patch(
    socket: Option<&std::path::Path>,
    root_repository: Uuid,
    recorded: &aq_delivery::result_work::Recorded,
    work: &Value,
    max_bytes: u64,
) -> Result<Value, AppError> {
    use aq_delivery::diff_patch::{self, PatchError};
    let Some(provenance) = work
        .get("provenance")
        .filter(|_| work["state"] == "pushed_ref")
    else {
        return Ok(unserved("no_retained_work"));
    };
    let (Some(base), Some(head)) = (provenance["base"].as_str(), provenance["head"].as_str())
    else {
        return Ok(unserved("no_retained_work"));
    };
    // The grant's repository is the report's root repository; any other
    // would carry a binding generation that is not its own.
    if recorded.repository_id.is_some_and(|r| r != root_repository) {
        return Ok(unserved("repository_mismatch"));
    }
    let Some(generation) = recorded.binding_generation else {
        return Ok(unserved("binding_unknown"));
    };
    let Some(socket) = socket else {
        return Ok(unserved("aq_git_not_configured"));
    };
    let repository = diff_patch::Repository {
        repository_id: root_repository,
        binding_generation: generation,
    };
    match diff_patch::read(socket, repository, base, head, max_bytes).await {
        Ok(patch) => Ok(json!({
            "served": true, "complete": true, "format": diff_patch::FORMAT,
            "base": patch.base, "head": patch.target, "size": patch.size,
            "sha256": patch.sha256,
            "provenance_digest": provenance["digest"],
            "encoding": "base64",
            "value": base64::engine::general_purpose::STANDARD.encode(&patch.bytes),
        })),
        Err(PatchError::Unsupported) => Ok(json!({
            "served": false, "reason": "unsupported",
            "requires": "aq-git capability diff.patch with limits.diff_patch_bytes",
            "retrieve": work["patch"]["retrieve"],
        })),
        Err(PatchError::LimitExceeded { limit }) => Ok(json!({
            "served": false, "reason": "limit_exceeded", "limit": limit,
        })),
        Err(PatchError::ObjectUnavailable) => Ok(unserved("object_unavailable")),
        Err(PatchError::BindingMoved) => Ok(unserved("binding_moved")),
        Err(PatchError::Protocol) => Ok(unserved("unverified")),
        Err(PatchError::Unavailable) => Err(AppError::Busy),
    }
}
/// The answer of `result.review.patch` for `project`'s exact `revision` of
/// `report_id`: [`AppError::NotFound`] for another project's report, an unknown
/// report or a revision that is not the report's digest.
pub async fn read_patch(
    s: &AppState,
    project: Uuid,
    report_id: Uuid,
    revision: &str,
    max_bytes: u64,
) -> Result<Value, AppError> {
    let retained = Retained::read(s, project, report_id, revision).await?;
    let mut work = retained.work();
    let socket = s
        .source_retention
        .as_ref()
        .map(|r| r.config.git_socket.as_path());
    let patch = retained_patch(
        socket,
        retained.item.repository_id,
        &retained.recorded,
        &work,
        max_bytes,
    )
    .await?;
    // `work.patch` says the bytes are not in a show answer; here they are
    // reported by `patch`, which is the only statement about them.
    if let Some(work) = work.as_object_mut() {
        work.remove("patch");
    }
    Ok(json!({
        "api_version": 1, "project_id": project, "coverage": retained.coverage,
        "report_id": retained.item.report_id, "revision": retained.item.revision,
        "work": work, "patch": patch,
    }))
}
async fn patch(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    use aq_delivery::diff_patch::MAX_PATCH_BYTES;
    let project = match authorize(&s, &h, PATCH).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match parse::<PatchQuery>(&body, |q| q.api_version) {
        Ok(q) => q,
        Err(r) => return r,
    };
    let max_bytes = q.max_bytes.unwrap_or(MAX_PATCH_BYTES);
    if !(1..=MAX_PATCH_BYTES).contains(&max_bytes) {
        return AgentError("invalid_request").into_response();
    }
    match bounded_by(
        &PATCH_BUDGET,
        25,
        read_patch(&s, project, q.report_id, &q.revision, max_bytes),
    )
    .await
    {
        Ok(v) => ok(v),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_delivery::result_work::{Recorded, Subject, describe, provenance_digest};
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // The reviewed result df3d2e44…@44a9f35e…: head from `delivery_pushed_results`,
    // base from its launch, no legacy source receipts.
    const REVISION: &str = "44a9f35e560162b48354d5374ec9c9ccc419a757ec82b5e0b52e18dbddbe8f89";
    const HEAD: &str = "105c94ad4a258ab7b943f14bbd31db935ec9af5e";
    const BASE: &str = "957a001e392c4405b00ea1f11a1e621b19842447";
    const TASK_REF: &str = "refs/heads/aq/f86d9749-4371-46ec-b302-815078c1718e";
    const REPOSITORY: &str = "674b55a3-94ec-55b4-8338-df9c4fc8a0ed";
    const PATCH: &[u8] = b"diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";

    #[derive(Clone, Copy, PartialEq)]
    enum Peer {
        Complete,
        /// An aq-git without `diff.patch` (the A3a release).
        Older,
        ObjectUnavailable,
        BindingMoved,
        /// Answers for another target commit.
        WrongTarget,
        /// A digest that is not the bytes'.
        BadDigest,
    }
    /// A synthetic aq-git; returns the requests it saw, in order.
    fn peer(socket: std::path::PathBuf, mode: Peer, advertised: u64) -> Arc<Mutex<Vec<Value>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (listener, record) = (
            tokio::net::UnixListener::bind(socket).unwrap(),
            seen.clone(),
        );
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                record.lock().unwrap().push(request.clone());
                let typed = |hex: &str| json!({"algorithm": "sha1", "hex": hex});
                let reply = match request["operation"].as_str().unwrap() {
                    "capabilities" if mode == Peer::Older => ok(
                        &request,
                        json!({"operations": ["capabilities", "diff", "contains"], "limits": {"query_response_bytes": 65536}}),
                    ),
                    "capabilities" => ok(
                        &request,
                        json!({"operations": ["capabilities", "diff", "diff.patch"], "limits": {"diff_patch_bytes": advertised}}),
                    ),
                    "diff.patch" => {
                        let params = &request["params"];
                        let wanted = params["max_bytes"].as_u64().unwrap_or(advertised);
                        let digest = hex::encode(Sha256::digest(PATCH));
                        match mode {
                            Peer::ObjectUnavailable => err(&request, "object_unavailable"),
                            Peer::BindingMoved => err(&request, "binding_conflict"),
                            _ if PATCH.len() as u64 > wanted => err(&request, "limit_exceeded"),
                            _ => ok(
                                &request,
                                json!({
                                    "format": "git_diff_binary_full_index_v1",
                                    "base": params["base"],
                                    "target": if mode == Peer::WrongTarget { typed(BASE) } else { params["target"].clone() },
                                    "size": PATCH.len(),
                                    "sha256": if mode == Peer::BadDigest { "0".repeat(64) } else { digest },
                                    "patch": {"encoding": "base64", "value": base64::engine::general_purpose::STANDARD.encode(PATCH)},
                                }),
                            ),
                        }
                    }
                    other => panic!("unexpected aq-git operation {other}"),
                };
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                reader.get_mut().write_all(&bytes).await.unwrap();
            }
        });
        seen
    }
    fn ok(request: &Value, result: Value) -> Value {
        json!({"protocol_version": 1, "request_id": request["request_id"], "ok": true, "result": result})
    }
    fn err(request: &Value, code: &str) -> Value {
        json!({"protocol_version": 1, "request_id": request["request_id"], "ok": false,
            "error": {"code": code, "message": "refused", "details": {}, "retry": "manual"}})
    }
    fn recorded() -> Recorded {
        Recorded {
            row: true,
            task_ref: Some(TASK_REF.into()),
            head: Some(HEAD.into()),
            repository_id: Some(REPOSITORY.parse().unwrap()),
            base: Some(BASE.into()),
            binding_generation: Some(3),
        }
    }
    fn work(revision: &str, recorded: &Recorded) -> Value {
        describe(
            &Subject {
                report_id: "df3d2e44-fdcb-42c9-aaaf-dbd47a1f9aa1".parse().unwrap(),
                revision,
                attempt_id: Uuid::from_u128(2),
                source_repository_id: REPOSITORY.parse().unwrap(),
                source_base: BASE,
                source_receipt_ids: &[],
            },
            recorded,
        )
    }
    /// A fresh socket path, kept alive by the returned directory.
    fn socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git.sock");
        (dir, path)
    }
    async fn read(
        socket: Option<&std::path::Path>,
        recorded: &Recorded,
        max_bytes: u64,
    ) -> Result<Value, AppError> {
        retained_patch(
            socket,
            REPOSITORY.parse().unwrap(),
            recorded,
            &work(REVISION, recorded),
            max_bytes,
        )
        .await
    }
    const MAX: u64 = aq_delivery::diff_patch::MAX_PATCH_BYTES;

    #[tokio::test]
    async fn the_reviewed_result_serves_its_exact_patch_bound_to_its_revision() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Complete, MAX);
        let recorded = recorded();
        let patch = read(Some(&path), &recorded, MAX).await.unwrap();
        assert_eq!(patch["served"], true);
        assert_eq!(patch["complete"], true);
        assert_eq!(patch["format"], "git_diff_binary_full_index_v1");
        assert_eq!(
            (patch["base"].as_str(), patch["head"].as_str()),
            (Some(BASE), Some(HEAD))
        );
        assert_eq!(patch["size"], PATCH.len());
        assert_eq!(patch["sha256"], hex::encode(Sha256::digest(PATCH)));
        assert_eq!(patch["encoding"], "base64");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(patch["value"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, PATCH);
        // The digest names this revision, repository, base, head and ref.
        let expected =
            provenance_digest(REVISION, REPOSITORY.parse().unwrap(), BASE, HEAD, TASK_REF);
        assert_eq!(patch["provenance_digest"], expected);
        // Another revision of the same report binds another digest.
        let other = REVISION.replace('4', "5");
        assert_ne!(
            work(&other, &recorded)["provenance"]["digest"],
            json!(expected)
        );

        // Capabilities first, then the query for exactly the recorded commits
        // of the root repository and its binding generation.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["operation"], "capabilities");
        let query = &seen[1];
        assert_eq!(query["operation"], "diff.patch");
        assert_eq!(query["repository_id"], REPOSITORY);
        assert_eq!(query["expected_binding_generation"], 3);
        assert_eq!(
            query["params"]["base"],
            json!({"algorithm": "sha1", "hex": BASE})
        );
        assert_eq!(
            query["params"]["target"],
            json!({"algorithm": "sha1", "hex": HEAD})
        );
        assert_eq!(query["params"]["max_bytes"], MAX);
    }

    #[tokio::test]
    async fn the_request_bound_never_exceeds_the_callers_or_the_advertised_limit() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Complete, 100_000);
        let patch = read(Some(&path), &recorded(), 50_000).await.unwrap();
        assert_eq!(patch["served"], true);
        assert_eq!(seen.lock().unwrap()[1]["params"]["max_bytes"], 50_000);
        read(Some(&path), &recorded(), MAX).await.unwrap();
        assert_eq!(seen.lock().unwrap()[3]["params"]["max_bytes"], 100_000);
    }

    #[tokio::test]
    async fn an_oversized_patch_serves_no_bytes_and_names_the_bound() {
        let (_dir, path) = socket();
        peer(path.clone(), Peer::Complete, MAX);
        let small = PATCH.len() as u64 - 1;
        let patch = read(Some(&path), &recorded(), small).await.unwrap();
        assert_eq!(patch["served"], false);
        assert_eq!(patch["reason"], "limit_exceeded");
        assert_eq!(patch["limit"], small);
        assert!(patch.get("value").is_none());
        // An advertised limit below the patch is the bound that applied.
        let (_dir, path) = socket();
        peer(path.clone(), Peer::Complete, small);
        let patch = read(Some(&path), &recorded(), MAX).await.unwrap();
        assert_eq!(
            (patch["reason"].as_str(), patch["limit"].as_u64()),
            (Some("limit_exceeded"), Some(small))
        );
    }

    #[tokio::test]
    async fn an_older_aq_git_is_reported_as_unsupported_and_is_not_queried() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Older, MAX);
        let patch = read(Some(&path), &recorded(), MAX).await.unwrap();
        assert_eq!(patch["served"], false);
        assert_eq!(patch["reason"], "unsupported");
        assert!(patch["requires"].as_str().unwrap().contains("diff.patch"));
        assert!(patch["retrieve"][2].as_str().unwrap().contains(HEAD));
        assert!(patch.get("value").is_none());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "only capabilities was asked");
        assert_eq!(seen[0]["operation"], "capabilities");
    }

    #[tokio::test]
    async fn missing_objects_a_moved_binding_and_a_contract_breach_serve_no_bytes() {
        for (mode, reason) in [
            (Peer::ObjectUnavailable, "object_unavailable"),
            (Peer::BindingMoved, "binding_moved"),
            (Peer::WrongTarget, "unverified"),
            (Peer::BadDigest, "unverified"),
        ] {
            let (_dir, path) = socket();
            peer(path.clone(), mode, MAX);
            let patch = read(Some(&path), &recorded(), MAX).await.unwrap();
            assert_eq!(patch["served"], false, "{reason}");
            assert_eq!(patch["reason"], reason);
            assert!(patch.get("value").is_none(), "{reason} exposed bytes");
        }
    }

    #[tokio::test]
    async fn work_that_is_not_retained_is_reported_and_aq_git_is_never_asked() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Complete, MAX);
        // No pushed row: the legacy-empty source receipts do not make a ref.
        let none = Recorded::default();
        assert_eq!(work(REVISION, &none)["state"], "missing");
        let patch = read(Some(&path), &none, MAX).await.unwrap();
        assert_eq!(
            patch,
            json!({"served": false, "reason": "no_retained_work"})
        );
        // A push that lost its lease retained no head.
        let lost = Recorded {
            head: None,
            ..recorded()
        };
        assert_eq!(
            read(Some(&path), &lost, MAX).await.unwrap()["reason"],
            "no_retained_work"
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn another_repository_an_unknown_binding_or_no_aq_git_serve_nothing() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Complete, MAX);
        let elsewhere = Recorded {
            repository_id: Some(Uuid::from_u128(9)),
            ..recorded()
        };
        assert_eq!(
            read(Some(&path), &elsewhere, MAX).await.unwrap()["reason"],
            "repository_mismatch"
        );
        let unbound = Recorded {
            binding_generation: None,
            ..recorded()
        };
        assert_eq!(
            read(Some(&path), &unbound, MAX).await.unwrap()["reason"],
            "binding_unknown"
        );
        assert_eq!(
            read(None, &recorded(), MAX).await.unwrap()["reason"],
            "aq_git_not_configured"
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_aq_git_is_a_retryable_failure_not_a_patch_state() {
        let (_dir, path) = socket();
        // Nothing listens at `path`.
        let result = read(Some(&path), &recorded(), MAX).await;
        assert!(matches!(result, Err(AppError::Busy)), "{result:?}");
    }
}
