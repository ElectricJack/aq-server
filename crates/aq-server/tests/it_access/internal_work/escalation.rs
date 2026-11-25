use super::{api, migrated, request};
use aq_internal_work::{
    FAILURE_BURST_WINDOW_MS, InternalWorkConfig, Purpose, Request, SUPERVISOR_SILENCE_MS, Way,
    service::{NotifySupervisor, Service},
    store::{self, Lineage, Row},
};
use aq_server::{InternalWork, test_support::TestDb};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const T0: i64 = 1_000_000_000;
const MINUTE: i64 = 60_000;

type Told = Arc<Mutex<Vec<(Option<Uuid>, String)>>>;

/// A service on a clock the test moves, telling a recorder instead of a
/// supervisor. No profile is configured, so every retry fails at once.
struct Fixture {
    _db: TestDb,
    pool: PgPool,
    clock: Arc<AtomicI64>,
    told: Told,
    service: Service,
}

async fn fixture() -> Fixture {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let clock = Arc::new(AtomicI64::new(T0));
    let told: Told = Arc::default();
    let sink = told.clone();
    let notify: NotifySupervisor = Arc::new(move |project, body| {
        sink.lock().unwrap().push((project, body));
        Box::pin(async { Ok(Uuid::new_v4()) })
    });
    let now = clock.clone();
    let service = Service::new(
        pool.clone(),
        Arc::new(move || now.load(Ordering::SeqCst)),
        None,
        None,
    )
    .unwrap()
    .notify_supervisor(notify);
    Fixture {
        _db: db,
        pool,
        clock,
        told,
        service,
    }
}

impl Fixture {
    fn at(&self, ms: i64) {
        self.clock.store(ms, Ordering::SeqCst);
    }
    /// A run that failed now: no candidate.
    async fn fail(&self, request: &Request) -> Uuid {
        let now = self.clock.load(Ordering::SeqCst);
        store::submit(&self.pool, request, &[], now).await.unwrap()
    }
    async fn row(&self, id: Uuid) -> Row {
        store::get(&self.pool, id).await.unwrap().unwrap()
    }
    async fn escalation(&self, id: Uuid) -> String {
        self.row(id).await.escalation
    }
    fn told(&self) -> Vec<(Option<Uuid>, String)> {
        self.told.lock().unwrap().clone()
    }
}

fn quiet(purpose: Purpose) -> Request {
    Request {
        purpose,
        escalate: false,
        ..request(false)
    }
}

fn loud() -> Request {
    Request {
        escalate: true,
        ..request(false)
    }
}

async fn mail(pool: &PgPool, thread: Uuid) -> Vec<(Value, String, bool)> {
    sqlx::query_as(
        "SELECT sender,body,central_global_mail_source_valid(message_id)
                 FROM mailbox_messages WHERE thread_id=$1",
    )
    .bind(thread)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn assert_notice(told_mail: &[(Value, String, bool)], project: Uuid, failed: Uuid) {
    let Some((sender, body, deliverable)) = told_mail
        .iter()
        .find(|(_, body, _)| body.contains(&failed.to_string()))
    else {
        panic!("{told_mail:?}")
    };
    assert_eq!(
        sender,
        &json!({"kind": "system", "component": "internal_work"})
    );
    assert!(deliverable, "a supervisor turn may take the message");
    let mount = if cfg!(feature = "supervisor_full_access") {
        "owner"
    } else {
        "admin"
    };
    for part in [
        &failed.to_string(),
        "Purpose: routing_classification",
        &format!("Affected item: task T-1 in project {project}"),
        "Reason: no_candidate: no candidate is configured for this purpose",
        &format!("/v1/supervisor/{mount}/api/internal-work/runs/{failed}/retry"),
        "Settings",
        &format!("/v1/supervisor/{mount}/api/internal-work/runs/{failed}/escalate"),
    ] {
        assert!(body.contains(part), "{part:?} in {body}");
    }
}

#[tokio::test]
async fn failed_run_with_escalate_messages_a_supervisor_once() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
        .bind(project)
        .bind(json!({"name": "Demo"}))
        .execute(&pool)
        .await
        .unwrap();
    let conversation = |project: Option<Uuid>| {
        let pool = pool.clone();
        async move {
            let id: Uuid = sqlx::query_scalar(
                "SELECT conversation_id FROM supervisor_conversation_ids
                 WHERE project_id IS NOT DISTINCT FROM $1",
            )
            .bind(project)
            .fetch_one(&pool)
            .await
            .unwrap();
            id
        }
    };
    let (scoped, global) = (conversation(Some(project)).await, conversation(None).await);
    assert_ne!(scoped, global);
    let config = InternalWorkConfig {
        api: vec![],
        poll_ms: 20,
        enabled: true,
    };
    let work =
        InternalWork::new(pool.clone(), aq_server::system_clock(), Some(&config), None).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mut about_project = loud();
    about_project.affected.project_id = Some(project);
    let failed = store::submit(&pool, &about_project, &[], now)
        .await
        .unwrap();
    let unscoped = store::submit(&pool, &loud(), &[], now).await.unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(work.clone().serve(rx));
    // Under full access the runs routes name no project, so the global
    // supervisor is told about both runs.
    let full = cfg!(feature = "supervisor_full_access");
    let (told, others) = if full { (global, 2) } else { (scoped, 1) };
    tokio::time::timeout(Duration::from_secs(10), async {
        while mail(&pool, told).await.is_empty() || mail(&pool, global).await.len() < others {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both supervisors are told");
    // Further passes of the loop tell nobody again.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(stop);
    assert_eq!(mail(&pool, scoped).await.len(), usize::from(!full));
    assert_eq!(mail(&pool, global).await.len(), others);
    assert_notice(&mail(&pool, told).await, project, failed);
    for id in [failed, unscoped] {
        let row = store::get(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.escalation, "supervisor");
    }
}

#[tokio::test]
async fn failed_run_without_escalate_raises_nothing() {
    let f = fixture().await;
    let mut ids = vec![];
    for _ in 0..4 {
        ids.push(f.fail(&quiet(Purpose::RoutingClassification)).await);
    }
    ids.push(f.fail(&quiet(Purpose::PlaybookCall)).await);
    assert_eq!(f.service.escalate().await.unwrap(), 0);
    f.at(T0 + SUPERVISOR_SILENCE_MS + MINUTE);
    assert_eq!(f.service.escalate().await.unwrap(), 0);
    assert!(f.told().is_empty());
    for id in ids {
        let row = f.row(id).await;
        assert_eq!((row.escalation.as_str(), row.diagnosis), ("none", None));
    }
}

#[tokio::test]
async fn five_failures_of_one_purpose_in_ten_minutes_escalate_once() {
    let f = fixture().await;
    let purpose = Purpose::RoutingClassification;
    let mut ids = vec![];
    for minute in 0..5 {
        f.at(T0 + minute * MINUTE);
        ids.push(f.fail(&quiet(purpose)).await);
        // Four failures of another purpose never add up to five.
        if minute < 4 {
            f.fail(&quiet(Purpose::PlaybookCall)).await;
        }
        let told = f.service.escalate().await.unwrap();
        assert_eq!(told, usize::from(minute == 4), "minute {minute}");
    }
    let newest = ids[4];
    assert_eq!(f.escalation(newest).await, "supervisor");
    for id in &ids[..4] {
        assert_eq!(f.escalation(*id).await, "none");
    }
    let told = f.told();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].0, None);
    assert!(told[0].1.contains(&newest.to_string()), "{}", told[0].1);
    assert!(
        told[0].1.contains("failed at least 5 times"),
        "{}",
        told[0].1
    );
    // A sixth failure within the window tells nobody.
    f.at(T0 + 5 * MINUTE);
    let sixth = f.fail(&quiet(purpose)).await;
    assert_eq!(f.service.escalate().await.unwrap(), 0);
    // Ten minutes after the escalation, five failures in the window do.
    f.at(T0 + 4 * MINUTE + FAILURE_BURST_WINDOW_MS + 1);
    for _ in 0..4 {
        f.fail(&quiet(purpose)).await;
    }
    assert_eq!(f.service.escalate().await.unwrap(), 1);
    assert_eq!(f.escalation(sixth).await, "none");
    assert_eq!(f.told().len(), 2);
}

#[tokio::test]
async fn third_failed_supervisor_retry_reaches_the_owner() {
    let f = fixture().await;
    let first = f.fail(&loud()).await;
    assert_eq!(f.service.escalate().await.unwrap(), 1);
    let mut current = first;
    for n in 1..=3 {
        let next = f.service.retry(current, true).await.unwrap();
        let (earlier, retry) = (f.row(current).await, f.row(next).await);
        assert_eq!(earlier.supervisor_attempts, n);
        assert_eq!(
            (
                retry.supervisor_attempts,
                retry.retry_of,
                retry.status.as_str()
            ),
            (n, Some(current), "failed")
        );
        let told = f.service.escalate().await.unwrap();
        let row = f.row(next).await;
        if n < 3 {
            assert_eq!((told, row.escalation.as_str()), (1, "supervisor"), "{n}");
            assert!(f.told().last().unwrap().1.contains(&format!("{n} of 3")));
        } else {
            assert_eq!((told, row.escalation.as_str()), (0, "owner"));
            assert_eq!(
                row.diagnosis.as_deref(),
                Some("supervisor attempts exhausted")
            );
        }
        current = next;
    }
    assert_eq!(f.told().len(), 3);
    // Runs a supervisor retried stay with it until the chain settles.
    assert_eq!(f.escalation(first).await, "supervisor");
    f.at(T0 + SUPERVISOR_SILENCE_MS);
    f.service.escalate().await.unwrap();
    assert_eq!(f.escalation(first).await, "supervisor");
    // The owner's own retry is not a supervisor attempt.
    let owners = f.service.retry(current, false).await.unwrap();
    assert_eq!(f.row(current).await.supervisor_attempts, 3);
    assert_eq!(f.row(owners).await.supervisor_attempts, 3);
}

#[tokio::test]
async fn supervisor_escalate_records_its_diagnosis_and_reaches_the_owner() {
    let f = fixture().await;
    let failed = f.fail(&loud()).await;
    f.service.escalate().await.unwrap();
    f.at(T0 + MINUTE);
    let diagnosis = "profile internal lists only a revoked key; the owner must add one";
    f.service
        .escalate_to_owner(failed, diagnosis)
        .await
        .unwrap();
    let row = f.row(failed).await;
    assert_eq!(
        (row.escalation.as_str(), row.diagnosis.as_deref()),
        ("owner", Some(diagnosis))
    );
    assert_eq!(row.escalated_at_ms, Some(T0 + MINUTE));
    // The owner already has it: silence changes nothing.
    f.at(T0 + MINUTE + SUPERVISOR_SILENCE_MS);
    f.service.escalate().await.unwrap();
    assert_eq!(f.row(failed).await.diagnosis.as_deref(), Some(diagnosis));
    for bad in ["", "  ", &"x".repeat(4001)] {
        let refused = f.service.escalate_to_owner(failed, bad).await;
        assert!(
            matches!(refused, Err(aq_internal_work::Error::Invalid(_))),
            "{refused:?}"
        );
    }
    assert!(
        f.service
            .escalate_to_owner(failed, &"x".repeat(4000))
            .await
            .is_ok()
    );
    let missing = f.service.escalate_to_owner(Uuid::new_v4(), "gone").await;
    assert!(matches!(missing, Err(aq_internal_work::Error::NotFound(_))));
    let queued = store::submit(&f.pool, &loud(), &[api("openai")], T0)
        .await
        .unwrap();
    let busy = f.service.escalate_to_owner(queued, "not failed").await;
    assert!(matches!(busy, Err(aq_internal_work::Error::Conflict(_))));
}

#[tokio::test]
async fn silent_supervisor_reaches_the_owner_after_thirty_minutes() {
    let f = fixture().await;
    let silent = f.fail(&loud()).await;
    let retried = f.fail(&loud()).await;
    assert_eq!(f.service.escalate().await.unwrap(), 2);
    f.at(T0 + 10 * MINUTE);
    let retry = f.service.retry(retried, true).await.unwrap();
    assert_eq!(f.service.escalate().await.unwrap(), 1);
    f.at(T0 + SUPERVISOR_SILENCE_MS - 1);
    f.service.escalate().await.unwrap();
    assert_eq!(f.escalation(silent).await, "supervisor");
    f.at(T0 + SUPERVISOR_SILENCE_MS);
    f.service.escalate().await.unwrap();
    let row = f.row(silent).await;
    assert_eq!(
        (row.escalation.as_str(), row.diagnosis.as_deref()),
        ("owner", Some("supervisor did not respond"))
    );
    // A retried run waits on its retry; the retry has its own 30 minutes.
    assert_eq!(f.escalation(retried).await, "supervisor");
    assert_eq!(f.escalation(retry).await, "supervisor");
    f.at(T0 + 10 * MINUTE + SUPERVISOR_SILENCE_MS);
    f.service.escalate().await.unwrap();
    assert_eq!(f.escalation(retried).await, "supervisor");
    assert_eq!(f.escalation(retry).await, "owner");
    assert_eq!(f.told().len(), 3);
}

#[tokio::test]
async fn answered_retry_resolves_the_original_and_removes_the_attention_item() {
    let f = fixture().await;
    let original = f.fail(&loud()).await;
    f.service.escalate().await.unwrap();
    f.service
        .escalate_to_owner(original, "no working model")
        .await
        .unwrap();
    // The owner retries; that retry fails and goes to a supervisor.
    let first = f.service.retry(original, false).await.unwrap();
    assert_eq!(f.service.escalate().await.unwrap(), 1);
    assert_eq!(f.escalation(first).await, "supervisor");
    // The supervisor fixes the cause and retries; this one answers.
    let lineage = Lineage {
        retry_of: first,
        supervisor_attempts: 1,
    };
    let second = store::Queue::at(&f.pool, T0)
        .submit_retry(&loud(), Ok(&[api("openai")]), lineage)
        .await
        .unwrap();
    let claimed = store::Queue::at(&f.pool, T0)
        .claim(Way::Api, &[], "r")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, second);
    let answer = json!({"choice": "small", "rationale": "one page"});
    store::Queue::at(&f.pool, T0 + 1)
        .complete(store::CandidateAttempt::new(second, 0), &answer, None)
        .await
        .unwrap();
    for id in [original, first] {
        assert_eq!(f.escalation(id).await, "resolved");
    }
    assert_eq!(f.escalation(second).await, "none");
    // The attention source lists runs with escalation 'owner': none is left.
    let owners: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_tasks WHERE escalation='owner'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(owners, 0);
    // A resolved or never-escalated run cannot be resolved again.
    for id in [original, second] {
        let refused = store::resolve(&f.pool, id).await;
        assert!(matches!(refused, Err(aq_internal_work::Error::Conflict(_))));
    }
}
