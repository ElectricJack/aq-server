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
        let refused = results::record_pushed(&f.state, &auth, &report, Some(&named)).await;
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
    let refused = results::record_pushed(&unanswered, &auth, &report, Some(&named)).await;
    assert_eq!(refused.map_err(|e| e.status().as_u16()).err(), Some(503));

    // Recorded at the reported head, with the ref as the result's source.
    *at.lock().unwrap() = Some(pushed.clone());
    results::record_pushed(&f.state, &auth, &report, Some(&named))
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
    results::record_pushed(&f.state, &auth, &report, Some(&named))
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 3);
    // Once a result names the head, the attempt's grant no longer serves pushes.
    let attempt = report.binding.attempt_id;
    let live = aq_delivery::git_grant::live(&pool, attempt, auth.id()).await;
    assert_eq!(live.unwrap(), None);
}

#[tokio::test]
async fn a_push_conflict_is_recorded_only_when_the_task_ref_left_its_lease() {
    use aq_playbook_domain::delivery_store::{DeliveryPort, PgDelivery};
    let (mut f, _, mut report) = prepare(true, true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let served = task_ref(&mut f, &mut report).await;
    // The grant's lease is "absent" and aq-git has no such ref: no conflict.
    let refused = results::record_pushed(&f.state, &auth, &report, Some(&Pushed::Conflict)).await;
    assert_eq!(refused.map_err(|e| e.status().as_u16()).err(), Some(409));
    // Another attempt pushed the branch meanwhile: the conflict is a fact.
    *served.at.lock().unwrap() = Some("7".repeat(40));
    results::record_pushed(&f.state, &auth, &report, Some(&Pushed::Conflict))
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
