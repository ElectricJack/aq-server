//! The exact retained patch of one result revision (`result.review.patch`):
//! the diff from the base to the head AQ recorded for it, read from aq-git's
//! `diff.patch` and checked (format, both OIDs, size, digest) before any byte
//! is returned. A read: it never approves, settles or recreates work.
use crate::diff_patch::{self, PatchError};
use crate::result_work::{self, Recorded, Subject};
use aq_dashboard_contract::result_metadata::{ResultMetadata, ResultPage};
use base64::Engine;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

/// aq-git did not answer: a retryable failure, not a state of the patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

/// `patch.served` is false with a `reason` whenever no verified bytes are
/// returned. Retained work that is absent is reported as evidence, not
/// recreated or approved.
fn unserved(reason: &str) -> Value {
    json!({"served": false, "reason": reason})
}
/// The report's root repository, recorded Git binding and described work.
pub struct WorkProvenance<'a> {
    pub root_repository: Uuid,
    pub recorded: &'a Recorded,
    pub work: &'a Value,
}

/// The `patch` object for the report's `work` (see `result_work::describe`).
/// `root_repository` is the report's source repository.
pub async fn retained_patch(
    socket: Option<&Path>,
    provenance: WorkProvenance<'_>,
    max_bytes: u64,
) -> Result<Value, Unavailable> {
    let WorkProvenance {
        root_repository,
        recorded,
        work,
    } = provenance;
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
    match diff_patch::read(
        socket,
        repository,
        diff_patch::CommitRange { base, target: head },
        max_bytes,
    )
    .await
    {
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
        Err(PatchError::Unavailable) => Err(Unavailable),
    }
}
/// One exact revision of a project's result and what the database recorded
/// of its retained work.
pub struct Retained {
    pub coverage: String,
    pub item: ResultMetadata,
    pub recorded: Recorded,
}
impl Retained {
    /// The only report of an exact-revision `page`, or `None` when it holds none.
    pub async fn from_page(
        db: impl sqlx::PgExecutor<'_>,
        page: ResultPage,
    ) -> sqlx::Result<Option<Self>> {
        let Some(item) = page.items.into_iter().next() else {
            return Ok(None);
        };
        let recorded = result_work::recorded(db, item.report_id).await?;
        Ok(Some(Self {
            coverage: page.coverage,
            item,
            recorded,
        }))
    }
    /// The `work` object of the revision (see [`result_work::describe`]).
    pub fn work(&self) -> Value {
        let item = &self.item;
        result_work::describe(
            &Subject {
                report_id: item.report_id,
                revision: &item.revision,
                attempt_id: item.attempt_id,
                source_repository_id: item.repository_id,
                source_base: item.source_base.hex.as_str(),
                source_receipt_ids: &item.source_receipt_ids,
            },
            &self.recorded,
        )
    }
    /// The `result.show` answer: metadata and the provenance of the work.
    pub fn show(&self, project: Uuid) -> Value {
        json!({
            "api_version": 1, "project_id": project, "coverage": self.coverage,
            "complete": true, "result": self.item, "work": self.work(),
        })
    }
    /// The `result.review.patch` answer. `work.patch` says the bytes are not
    /// in a show answer; here `patch` is the only statement about them.
    pub async fn patch(
        &self,
        project: Uuid,
        socket: Option<&Path>,
        max_bytes: u64,
    ) -> Result<Value, Unavailable> {
        let mut work = self.work();
        let patch = retained_patch(
            socket,
            WorkProvenance {
                root_repository: self.item.repository_id,
                recorded: &self.recorded,
                work: &work,
            },
            max_bytes,
        )
        .await?;
        if let Some(work) = work.as_object_mut() {
            work.remove("patch");
        }
        Ok(json!({
            "api_version": 1, "project_id": project, "coverage": self.coverage,
            "report_id": self.item.report_id, "revision": self.item.revision,
            "work": work, "patch": patch,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_work::{Recorded, RetainedRef, Subject, describe, provenance_digest};
    use base64::Engine;
    use serde_json::Value;
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
                let reply = peer_reply(&request, mode, advertised);
                let mut bytes = serde_json::to_vec(&reply).unwrap();
                bytes.push(b'\n');
                reader.get_mut().write_all(&bytes).await.unwrap();
            }
        });
        seen
    }
    fn peer_reply(request: &Value, mode: Peer, advertised: u64) -> Value {
        match request["operation"].as_str().unwrap() {
            "capabilities" if mode == Peer::Older => ok(
                request,
                json!({"operations": ["capabilities", "diff", "contains"], "limits": {"query_response_bytes": 65536}}),
            ),
            "capabilities" => ok(
                request,
                json!({"operations": ["capabilities", "diff", "diff.patch"], "limits": {"diff_patch_bytes": advertised}}),
            ),
            "diff.patch" => patch_reply(request, mode, advertised),
            other => panic!("unexpected aq-git operation {other}"),
        }
    }
    fn patch_reply(request: &Value, mode: Peer, advertised: u64) -> Value {
        let typed = |hex: &str| json!({"algorithm": "sha1", "hex": hex});
        let params = &request["params"];
        let wanted = params["max_bytes"].as_u64().unwrap_or(advertised);
        let digest = hex::encode(Sha256::digest(PATCH));
        match mode {
            Peer::ObjectUnavailable => err(request, "object_unavailable"),
            Peer::BindingMoved => err(request, "binding_conflict"),
            _ if PATCH.len() as u64 > wanted => err(request, "limit_exceeded"),
            _ => ok(
                request,
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
    ) -> Result<Value, Unavailable> {
        retained_patch(
            socket,
            WorkProvenance {
                root_repository: REPOSITORY.parse().unwrap(),
                recorded,
                work: &work(REVISION, recorded),
            },
            max_bytes,
        )
        .await
    }
    const MAX: u64 = crate::diff_patch::MAX_PATCH_BYTES;

    #[tokio::test]
    async fn the_reviewed_result_serves_its_exact_patch_bound_to_its_revision() {
        let (_dir, path) = socket();
        let seen = peer(path.clone(), Peer::Complete, MAX);
        let recorded = recorded();
        let patch = read(Some(&path), &recorded, MAX).await.unwrap();
        assert_eq!(
            (
                patch["served"].as_bool(),
                patch["complete"].as_bool(),
                patch["format"].as_str()
            ),
            (
                Some(true),
                Some(true),
                Some("git_diff_binary_full_index_v1")
            )
        );
        assert_eq!(
            (patch["base"].as_str(), patch["head"].as_str()),
            (Some(BASE), Some(HEAD))
        );
        let digest = hex::encode(Sha256::digest(PATCH));
        assert_eq!(
            (
                patch["size"].as_u64(),
                patch["sha256"].as_str(),
                patch["encoding"].as_str()
            ),
            (
                Some(PATCH.len() as u64),
                Some(digest.as_str()),
                Some("base64")
            )
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(patch["value"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, PATCH);
        // The digest names this revision, repository, base, head and ref.
        let expected = provenance_digest(
            REVISION,
            REPOSITORY.parse().unwrap(),
            RetainedRef {
                base: BASE,
                head: HEAD,
                task_ref: TASK_REF,
            },
        );
        assert_eq!(patch["provenance_digest"], expected);
        // Another revision of the same report binds another digest.
        let other = REVISION.replace('4', "5");
        assert_ne!(
            work(&other, &recorded)["provenance"]["digest"],
            json!(expected)
        );

        // Capabilities first, then the query for exactly the recorded commits
        // of the root repository and its binding generation.
        check_bound_request(&seen.lock().unwrap());
    }

    fn check_bound_request(seen: &[Value]) {
        let [capabilities, query] = seen else {
            panic!("expected capabilities and a patch request: {seen:?}")
        };
        assert_eq!(capabilities["operation"], "capabilities");
        assert_eq!(
            (
                query["operation"].as_str(),
                query["repository_id"].as_str(),
                query["expected_binding_generation"].as_u64()
            ),
            (Some("diff.patch"), Some(REPOSITORY), Some(3))
        );
        for (field, oid) in [("base", BASE), ("target", HEAD)] {
            assert_eq!(
                query["params"][field],
                json!({"algorithm":"sha1","hex":oid})
            );
        }
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
        assert_eq!(
            (
                patch["served"].as_bool(),
                patch["reason"].as_str(),
                patch["limit"].as_u64()
            ),
            (Some(false), Some("limit_exceeded"), Some(small))
        );
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
        assert_eq!(
            (
                patch["served"].as_bool(),
                patch["reason"].as_str(),
                patch.get("value")
            ),
            (Some(false), Some("unsupported"), None)
        );
        assert!(patch["requires"].as_str().unwrap().contains("diff.patch"));
        assert!(patch["retrieve"][2].as_str().unwrap().contains(HEAD));
        let seen = seen.lock().unwrap();
        let [single] = seen.as_slice() else {
            panic!("only capabilities should be asked: {seen:?}")
        };
        assert_eq!(single["operation"], "capabilities");
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
        assert!(matches!(result, Err(Unavailable)), "{result:?}");
    }
}
