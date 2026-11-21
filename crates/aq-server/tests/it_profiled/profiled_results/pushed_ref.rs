//! Spec 0016 §5.1 (feature `delivery_playbook`): a result report names the
//! task branch the node pushed and its commit. The server records it only
//! when central aq-git's `ref.show` has the attempt's task ref at that commit;
//! the ref is the record of the result, so its source is retained.
use super::{Fixture, ProfiledExecutionSpec, ResultReport, context, prepare, results};
use aq_delivery::git_grant::{Pushed, PushedRef};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A synthetic aq-git that answers `ref.show` with `head` for `task_ref`
/// (and nothing for any other ref) and counts its calls.
fn ref_show_peer(
    socket: std::path::PathBuf,
    task_ref: String,
    head: Arc<Mutex<Option<String>>>,
) -> Arc<Mutex<usize>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let calls = Arc::new(Mutex::new(0));
    let (listener, counted) = (
        tokio::net::UnixListener::bind(socket).unwrap(),
        calls.clone(),
    );
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["operation"], "ref.show");
            *counted.lock().unwrap() += 1;
            let at = head
                .lock()
                .unwrap()
                .clone()
                .filter(|_| request["params"]["ref"] == task_ref);
            let oid = at.map(|hex| json!({"algorithm": "sha1", "hex": hex}));
            let result = json!({"ref": request["params"]["ref"], "oid": oid});
            let reply = json!({"protocol_version": 1, "request_id": request["request_id"], "ok": true, "result": result});
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        }
    });
    calls
}

/// A recorded git grant for the report's attempt, and aq-git's `ref.show`
/// answering for its task ref. The report names no bundle receipt.
/// The result request headers that name `pushed`.
pub(crate) fn headers(pushed: &Pushed) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    let mut set = |name: &'static str, value: &str| headers.insert(name, value.parse().unwrap());
    match pushed {
        Pushed::At(at) => {
            set("aq-pushed-ref", &at.reference);
            set("aq-pushed-head", &at.head);
        }
        Pushed::Conflict => {
            set("aq-push-conflict", "true");
        }
    }
    headers
}
pub(crate) struct TaskRef {
    pub task_ref: String,
    /// Where aq-git has the task ref (`None`: absent).
    pub at: Arc<Mutex<Option<String>>>,
    pub calls: Arc<Mutex<usize>>,
    _directory: tempfile::TempDir,
}
impl TaskRef {
    fn dir(&self) -> &std::path::Path {
        self._directory.path()
    }
}
pub(crate) async fn task_ref(f: &mut Fixture, report: &mut ResultReport) -> TaskRef {
    report.source_receipt_ids.clear();
    let pool = f.db.pool().await;
    let spec: Value =
        sqlx::query_scalar("SELECT execution_spec FROM profiled_launches WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let spec: ProfiledExecutionSpec = serde_json::from_value(spec).unwrap();
    let task_ref = format!("refs/heads/aq/{}", spec.task.task_id);
    sqlx::query("INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref) VALUES($1,$2,$3,$4)")
        .bind(report.binding.attempt_id)
        .bind(spec.source.repository_id)
        .bind(&spec.source.resolved_object.hex)
        .bind(&task_ref)
        .execute(&pool)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("git.sock");
    let at = Arc::new(Mutex::new(None));
    let calls = ref_show_peer(socket.clone(), task_ref.clone(), at.clone());
    f.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                staging_root: directory.path().join("staging"),
                git_root: directory.path().join("git"),
                git_socket: socket,
                max_staging_bytes: 1048576,
                max_bundle_bytes: 1048576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
                git_deadline_ms: 30_000,
            },
        )
        .unwrap(),
    ));
    TaskRef {
        task_ref,
        at,
        calls,
        _directory: directory,
    }
}

#[tokio::test]
async fn a_report_is_recorded_only_when_its_task_ref_is_at_the_reported_head() {
    let (mut f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let served = task_ref(&mut f, &mut report).await;
    let (task_ref, at, calls) = (&served.task_ref, &served.at, &served.calls);
    let pushed = "4".repeat(40);
    let recorded = || async {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE report_id=$1")
                .bind(report.report_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        n
    };
    let name = |reference: &str| {
        Pushed::At(PushedRef {
            reference: reference.to_owned(),
            head: pushed.clone(),
        })
    };

    // Refused: a ref outside aq/ (400), another task's ref, a missing ref, and
    // the task ref at another commit (409); with aq-git silent, 503.
    let elsewhere = format!("refs/heads/aq/{}", Uuid::new_v4());
    for (reference, head, status) in [
        ("refs/heads/main", Some(pushed.clone()), 400),
        (elsewhere.as_str(), Some(pushed.clone()), 409),
        (task_ref.as_str(), None, 409),
        (task_ref.as_str(), Some("5".repeat(40)), 409),
    ] {
        *at.lock().unwrap() = head;
        let named = name(reference);
        let refused = results::record_pushed(&f.state, &auth, &report, &headers(&named)).await;
        let status_of = refused.as_ref().map_err(|e| e.status().as_u16());
        assert_eq!(status_of.err(), Some(status), "{reference}: {refused:?}");
    }
    assert_eq!(recorded().await, 0);
    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "only the attempt's own task ref is looked up"
    );
    let config = (f.state.source_retention.as_ref().unwrap().config).clone();
    let mut silent = config.clone();
    silent.git_socket = served.dir().join("silent.sock");
    let runtime = aq_server::source_retention::Runtime::new(silent).unwrap();
    let mut unanswered = f.state.clone();
    unanswered.source_retention = Some(Arc::new(runtime));
    let named = name(task_ref);
    let refused = results::record_pushed(&unanswered, &auth, &report, &headers(&named)).await;
    assert_eq!(refused.map_err(|e| e.status().as_u16()).err(), Some(503));

    // Recorded at the reported head, with the ref as the result's source.
    *at.lock().unwrap() = Some(pushed.clone());
    results::record_pushed(&f.state, &auth, &report, &headers(&named))
        .await
        .unwrap();
    assert_eq!(recorded().await, 1);
    let facts: Value =
        sqlx::query_scalar("SELECT facts FROM delivery_result_facts WHERE report_id=$1")
            .bind(report.report_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let source = json!({"operation_id": report.report_id, "checkpoint": pushed, "changed": true});
    assert_eq!(facts["source"], source);
    // A replay of the recorded report is not asked again, even after the ref moved.
    *at.lock().unwrap() = Some("6".repeat(40));
    results::record_pushed(&f.state, &auth, &report, &headers(&named))
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 3);
    // Once a result names the head, the attempt's grant no longer serves pushes.
    let attempt = report.binding.attempt_id;
    let live = aq_delivery::git_grant::live(&pool, attempt, auth.id()).await;
    assert_eq!(live.unwrap(), None);
    use aq_delivery::git_grant::{Lookup, lookup};
    assert!(matches!(
        lookup(&pool, attempt, auth.id()).await.unwrap(),
        Lookup::Granted(_)
    ));
}

#[tokio::test]
async fn a_push_conflict_is_recorded_only_when_the_task_ref_left_its_lease() {
    use aq_playbook_domain::delivery_store::{DeliveryPort, PgDelivery};
    let (mut f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let served = task_ref(&mut f, &mut report).await;
    // The grant's lease is "absent" and aq-git has no such ref: no conflict.
    let refused =
        results::record_pushed(&f.state, &auth, &report, &headers(&Pushed::Conflict)).await;
    assert_eq!(refused.map_err(|e| e.status().as_u16()).err(), Some(409));
    // Another attempt pushed the branch meanwhile: the conflict is a fact.
    *served.at.lock().unwrap() = Some("7".repeat(40));
    results::record_pushed(&f.state, &auth, &report, &headers(&Pushed::Conflict))
        .await
        .unwrap();
    let facts: Value =
        sqlx::query_scalar("SELECT facts FROM delivery_result_facts WHERE report_id=$1")
            .bind(report.report_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(facts["source"], Value::Null, "a conflict retains no source");
    let task: Uuid = served.task_ref["refs/heads/aq/".len()..].parse().unwrap();
    let latest = PgDelivery(pool.clone())
        .latest(task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (latest.outcome.as_str(), latest.head.as_str()),
        ("push_conflict", "")
    );
}

/// A reviewer reads a recorded result's retained work by `report_id`: the
/// pushed head and the grant's base, bound to the exact revision. A changed
/// revision, another project and a report with no retained row are refused or
/// reported as missing, never answered with someone else's work.
#[tokio::test]
async fn a_pushed_result_names_its_retained_head_and_refuses_other_revisions_and_projects() {
    use aq_delivery::result_work::{Subject, describe, recorded};
    use aq_server::profiled_execution::human_results;
    let (mut f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let served = task_ref(&mut f, &mut report).await;
    let pushed = "4".repeat(40);
    *served.at.lock().unwrap() = Some(pushed.clone());
    let named = Pushed::At(PushedRef {
        reference: served.task_ref.clone(),
        head: pushed.clone(),
    });
    results::record_pushed(&f.state, &auth, &report, &headers(&named))
        .await
        .unwrap();
    let project = f.selection.lineage.project_id;
    let page = human_results::read_project(&f.state, project, None, None, None)
        .await
        .unwrap();
    let item = &page.items[0];
    assert_eq!(item.report_id, report.report_id);
    let work = |item: &human_results::ResultMetadata, recorded: &_| {
        describe(
            &Subject {
                report_id: item.report_id,
                revision: &item.revision,
                attempt_id: item.attempt_id,
                source_repository_id: item.repository_id,
                source_base: item.source_base.hex.as_str(),
                source_receipt_ids: &item.source_receipt_ids,
            },
            recorded,
        )
    };
    let found = recorded(&pool, item.report_id).await.unwrap();
    let described = work(item, &found);
    assert_eq!(described["state"], "pushed_ref");
    assert_eq!(described["provenance"]["head"], pushed);
    assert_eq!(described["provenance"]["base"], item.source_base.hex);
    assert_eq!(described["provenance"]["ref"], served.task_ref);
    assert_eq!(
        described["provenance"]["repository_id"],
        json!(item.repository_id)
    );
    // The digest binds the revision: another revision cannot reuse it.
    let other = aq_delivery::result_work::provenance_digest(
        &"0".repeat(64),
        item.repository_id,
        &item.source_base.hex,
        &pushed,
        &served.task_ref,
    );
    assert_ne!(described["provenance"]["digest"], json!(other));

    // A changed revision of the same report, and another project, are refused.
    let exact = async |project, revision: &str| {
        human_results::read_project(
            &f.state,
            project,
            None,
            None,
            Some((report.report_id, revision)),
        )
        .await
    };
    assert!(exact(project, &item.revision).await.is_ok());
    assert!(exact(project, &"0".repeat(64)).await.is_err());
    assert!(exact(Uuid::new_v4(), &item.revision).await.is_err());
    // A report with no `delivery_pushed_results` row and no receipts has no
    // retained work: it says `missing` and recreates nothing.
    let none = recorded(&pool, Uuid::new_v4()).await.unwrap();
    let missing = work(item, &none);
    assert_eq!(missing["state"], "missing");
    assert!(missing.get("provenance").is_none());
    assert_eq!(missing["patch"]["served"], false);
}

/// A synthetic aq-git that answers `capabilities` and `diff.patch` (aq-git
/// PR 24, `git_diff_binary_full_index_v1`) for any repository and returns the
/// operations it saw. `older` lacks the `diff.patch` capability.
fn diff_patch_peer(
    socket: std::path::PathBuf,
    older: bool,
    patch: &'static [u8],
) -> Arc<Mutex<Vec<Value>>> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
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
            let result = match request["operation"].as_str().unwrap() {
                "capabilities" if older => {
                    json!({"operations": ["capabilities", "diff"], "limits": {}})
                }
                "capabilities" => {
                    json!({"operations": ["capabilities", "diff", "diff.patch"], "limits": {"diff_patch_bytes": 4194304}})
                }
                "diff.patch" => json!({
                    "format": "git_diff_binary_full_index_v1",
                    "base": request["params"]["base"], "target": request["params"]["target"],
                    "size": patch.len(), "sha256": hex::encode(Sha256::digest(patch)),
                    "patch": {"encoding": "base64", "value": base64::engine::general_purpose::STANDARD.encode(patch)},
                }),
                other => panic!("unexpected aq-git operation {other}"),
            };
            let reply = json!({"protocol_version": 1, "request_id": request["request_id"], "ok": true, "result": result});
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        }
    });
    seen
}

fn runtime_at(
    directory: &std::path::Path,
    socket: std::path::PathBuf,
) -> Arc<aq_server::source_retention::Runtime> {
    Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                staging_root: directory.join("staging"),
                git_root: directory.join("git"),
                git_socket: socket,
                max_staging_bytes: 1048576,
                max_bundle_bytes: 1048576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
                git_deadline_ms: 30_000,
            },
        )
        .unwrap(),
    )
}

/// A reviewer of the result's own project reads the exact retained patch
/// between the recorded base and head through aq-git's `diff.patch`, bound to
/// the exact revision. Another project, a changed revision and an unknown
/// report are `NotFound`; a patch over the caller's bound and an aq-git
/// without the capability serve no bytes and say why.
#[tokio::test]
async fn a_reviewer_reads_the_exact_retained_patch_of_its_own_project_only() {
    use aq_server::agent_access::results::read_patch;
    use aq_server::error::AppError;
    use base64::Engine;
    const PATCH: &[u8] = b"diff --git a/a b/a\nindex 1111111..2222222 100644\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n";
    let (mut f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let served = task_ref(&mut f, &mut report).await;
    let pushed = "4".repeat(40);
    *served.at.lock().unwrap() = Some(pushed.clone());
    let named = Pushed::At(PushedRef {
        reference: served.task_ref.clone(),
        head: pushed.clone(),
    });
    results::record_pushed(&f.state, &auth, &report, &headers(&named))
        .await
        .unwrap();
    let project = f.selection.lineage.project_id;
    let page = aq_server::profiled_execution::human_results::read_project(
        &f.state, project, None, None, None,
    )
    .await
    .unwrap();
    let item = &page.items[0];
    let (revision, base) = (item.revision.clone(), item.source_base.hex.clone());

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("diff.sock");
    let seen = diff_patch_peer(socket.clone(), false, PATCH);
    f.state.source_retention = Some(runtime_at(directory.path(), socket));
    let max = aq_delivery::diff_patch::MAX_PATCH_BYTES;

    let read = read_patch(&f.state, project, report.report_id, &revision, max)
        .await
        .unwrap();
    let patch = &read["patch"];
    assert_eq!(patch["served"], true, "{read}");
    assert_eq!(patch["complete"], true);
    assert_eq!(patch["base"], base);
    assert_eq!(patch["head"], pushed);
    assert_eq!(patch["size"], PATCH.len());
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(patch["value"].as_str().unwrap())
            .unwrap(),
        PATCH
    );
    assert_eq!(
        patch["provenance_digest"],
        read["work"]["provenance"]["digest"]
    );
    assert_eq!(read["revision"], revision);
    {
        let seen = seen.lock().unwrap();
        let query = seen
            .iter()
            .find(|r| r["operation"] == "diff.patch")
            .unwrap();
        assert_eq!(query["repository_id"], json!(item.repository_id));
        assert_eq!(query["params"]["base"]["hex"], base);
        assert_eq!(query["params"]["target"]["hex"], pushed);
    }

    // Another project, a changed revision and an unknown report: not found,
    // and aq-git is not asked for them.
    let asked = seen.lock().unwrap().len();
    for (project, report_id, revision) in [
        (Uuid::new_v4(), report.report_id, revision.clone()),
        (project, report.report_id, "0".repeat(64)),
        (project, Uuid::new_v4(), revision.clone()),
    ] {
        let refused = read_patch(&f.state, project, report_id, &revision, max).await;
        assert!(matches!(refused, Err(AppError::NotFound(_))), "{refused:?}");
    }
    assert_eq!(seen.lock().unwrap().len(), asked);

    // Over the caller's bound: no bytes, and the bound is named.
    let small = read_patch(&f.state, project, report.report_id, &revision, 8)
        .await
        .unwrap();
    assert_eq!(small["patch"]["served"], false);
    assert_eq!(small["patch"]["reason"], "limit_exceeded");
    assert_eq!(small["patch"]["limit"], 8);
    assert!(small["patch"].get("value").is_none());

    // An older aq-git is reported as unsupported.
    let older = tempfile::tempdir().unwrap();
    let socket = older.path().join("older.sock");
    diff_patch_peer(socket.clone(), true, PATCH);
    f.state.source_retention = Some(runtime_at(older.path(), socket));
    let unsupported = read_patch(&f.state, project, report.report_id, &revision, max)
        .await
        .unwrap();
    assert_eq!(unsupported["patch"]["served"], false);
    assert_eq!(unsupported["patch"]["reason"], "unsupported");
    assert_eq!(unsupported["work"]["state"], "pushed_ref");
}
