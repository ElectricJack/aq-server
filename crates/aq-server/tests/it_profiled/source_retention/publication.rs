//! D1b: policy-driven delivery, typed refusals and §10.3 reconciliation.
use super::delivery::{candidate, context, peer, ready, response_peer, retain, service};
use super::*;
#[cfg(feature = "delivery_playbook")]
use aq_server::delivery::{
    BranchDeletion, Checks, GitHubRepository, MergeMethod, MergeOutcome, PullRequest,
    github::{GitHubApiError, GitHubResult},
};
#[cfg(not(feature = "delivery_playbook"))]
use aq_server::delivery::{ChecksState, GitHubError, PullRequest, PullRequestSpec};
use aq_server::delivery::{
    DeliverRequest, DeliveryError, DeliveryOutcome, DeliveryService, GitHubAdapter,
    ObservationRequest, PublicationMode, PublicationPolicy, ReconcileRequest, RefusalReason,
    Resolution, ResolveDisposition, ResolveRequest,
};
use std::sync::Mutex;

#[derive(Default)]
struct FakeGitHub {
    calls: Mutex<Vec<&'static str>>,
}
#[cfg(not(feature = "delivery_playbook"))]
#[async_trait::async_trait]
impl GitHubAdapter for FakeGitHub {
    async fn open_pull_request(
        &self,
        _: &PullRequestSpec,
    ) -> std::result::Result<PullRequest, GitHubError> {
        self.calls.lock().unwrap().push("open_pull_request");
        Err(GitHubError("fake".into()))
    }
    async fn checks(&self, _: &PullRequest) -> std::result::Result<ChecksState, GitHubError> {
        self.calls.lock().unwrap().push("checks");
        Ok(ChecksState::Pending)
    }
}
#[cfg(feature = "delivery_playbook")]
impl FakeGitHub {
    fn call<T>(&self, operation: &'static str) -> GitHubResult<T> {
        self.calls.lock().unwrap().push(operation);
        Err(GitHubApiError::Unconfigured)
    }
}
#[cfg(feature = "delivery_playbook")]
#[async_trait::async_trait]
impl GitHubAdapter for FakeGitHub {
    async fn open_or_find(
        &self,
        _: &GitHubRepository,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> GitHubResult<PullRequest> {
        self.call("open_or_find")
    }
    async fn pull_request(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        self.call("pull_request")
    }
    async fn checks(
        &self,
        _: &GitHubRepository,
        _: u64,
        _: Option<&[String]>,
    ) -> GitHubResult<Checks> {
        self.call("checks")
    }
    async fn merge(
        &self,
        _: &GitHubRepository,
        _: u64,
        _: MergeMethod,
        _: &str,
        _: &str,
        _: aq_server::delivery::QueueWait,
    ) -> GitHubResult<MergeOutcome> {
        self.call("merge")
    }
    async fn close(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        self.call("close")
    }
    async fn delete_branch(&self, _: &GitHubRepository, _: &str) -> GitHubResult<BranchDeletion> {
        self.call("delete_branch")
    }
}
fn policy(mode: PublicationMode, target: &str) -> PublicationPolicy {
    PublicationPolicy {
        mode,
        target: target.into(),
        wait_for_checks: None,
    }
}
fn unbind(h: &Harness) {
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
}
async fn ready_candidate(h: &Harness, svc: &DeliveryService) -> Uuid {
    ready_candidate_from(h, svc, retain(h).await).await
}
async fn ready_candidate_from(h: &Harness, svc: &DeliveryService, receipt: Uuid) -> Uuid {
    let r = candidate(h, receipt);
    let task = peer(h, ready(h, r.context.operation_id)).await;
    svc.create_candidate(r.clone()).await.unwrap();
    task.await.unwrap();
    unbind(h);
    r.context.operation_id
}
fn deliver_request(h: &Harness, candidate_id: Uuid) -> DeliverRequest {
    DeliverRequest {
        context: context(h),
        candidate_id,
        expected_old: h.manifest.source.base.clone(),
    }
}
async fn holds(h: &Harness, operation: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM delivery_destinations WHERE operation_id=$1")
        .bind(operation)
        .fetch_one(&h.pool)
        .await
        .unwrap()
}
fn indeterminate(operation: Uuid, target: &str) -> Value {
    json!({"ok":false,"operation_state":"pending","error":{"code":"operation_indeterminate","message":"publication was dispatched; reconcile before any new attempt","details":{"operation_id":operation,"target":target},"retry":"reconcile_first"}})
}
async fn generation(h: &Harness, svc: &DeliveryService, target: Uuid) -> u64 {
    let observation = ObservationRequest {
        context: context(h),
        target_id: target,
    };
    let task = peer(h, json!({"version":1,"id":target,"generation":4,"status":"pending","phase":"dispatched","payload":{"repository_id":h.manifest.source.repository_id,"expected_binding_generation":1},"payload_hash":"a".repeat(64),"result":null,"error":null,"metadata":{}})).await;
    let shown = svc.operation_show(observation).await.unwrap();
    task.await.unwrap();
    unbind(h);
    shown.outcome.unwrap().generation
}

#[tokio::test]
async fn ambiguous_push_reconciles_without_double_publish() {
    let h = Harness::new().await;
    let svc = service(&h);
    let github = FakeGitHub::default();
    let candidate_id = ready_candidate(&h, &svc).await;
    let main = policy(PublicationMode::FastForward, "refs/heads/main");
    let mut publishes_of_original = 0;

    // A dispatched publish without acknowledgement is ambiguous and holds the destination.
    let original = deliver_request(&h, candidate_id);
    let op = original.context.operation_id;
    let task = response_peer(&h, indeterminate(op, "refs/heads/main")).await;
    let outcome = svc.deliver(&main, original.clone(), &github).await.unwrap();
    let sent = task.await.unwrap();
    unbind(&h);
    assert_eq!(sent["operation"], "publish");
    publishes_of_original += 1;
    assert!(matches!(outcome, DeliveryOutcome::Ambiguous { operation_id } if operation_id == op));
    assert_eq!(holds(&h, op).await, 1);

    // No socket is bound: any resend would surface as a transport/indeterminate
    // failure rather than these typed fenced answers.
    let fresh = deliver_request(&h, candidate_id);
    assert!(matches!(
        svc.deliver(&main, fresh, &github).await.unwrap(),
        DeliveryOutcome::DestinationBlocked
    ));
    assert!(matches!(
        svc.deliver(&main, original.clone(), &github).await.unwrap(),
        DeliveryOutcome::Ambiguous { .. }
    ));

    let checks = PublicationCheck {
        h: &h,
        svc: &svc,
        policy: &main,
        github: &github,
    };
    checks.reconcile(original).await;
    // Later publication needs a new operation ID and a fresh expected_old.
    let mut later = deliver_request(&h, candidate_id);
    later.expected_old = h.manifest.checkpoint.clone();
    let task = peer(&h, json!({"outcome":"already_present","observed_target":h.manifest.checkpoint,"observed_at":"2026-11-04T00:00:01Z","caused_by_this_operation":false})).await;
    let outcome = svc.deliver(&main, later.clone(), &github).await.unwrap();
    let sent = task.await.unwrap();
    unbind(&h);
    assert_eq!(sent["operation_id"], json!(later.context.operation_id));
    assert_eq!(sent["params"]["expected_old"], json!(h.manifest.checkpoint));
    assert!(matches!(outcome, DeliveryOutcome::ObservedPresent { .. }));

    assert_owner_resolution(&h, &svc, &github, candidate_id).await;
    assert_eq!(publishes_of_original, 1, "original publish dispatched once");
    let dispatched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_publish_dispatches WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(dispatched, 1);
    assert_eq!(*github.calls.lock().unwrap(), Vec::<&str>::new());
}

#[tokio::test]
async fn publication_conflict_reports_typed_refusal() {
    let h = Harness::new().await;
    let svc = service(&h);
    let github = FakeGitHub::default();
    let receipt = retain(&h).await;
    let candidate_id = ready_candidate_from(&h, &svc, receipt).await;
    let main = policy(PublicationMode::FastForward, "refs/heads/main");
    let moved = json!({"algorithm":"sha1","hex":"c".repeat(40)});
    let cases = refusal_cases(&moved);
    for (reply, reason, observed, detail) in cases {
        let r = deliver_request(&h, candidate_id);
        let task = response_peer(&h, reply).await;
        let outcome = svc.deliver(&main, r.clone(), &github).await.unwrap();
        task.await.unwrap();
        unbind(&h);
        let DeliveryOutcome::Refused(refusal) = outcome else {
            panic!("expected typed refusal for {reason:?}, got {outcome:?}");
        };
        assert_eq!(refusal.reason, reason);
        assert_eq!(refusal.expected_old.as_ref(), Some(&r.expected_old));
        assert_eq!(
            refusal.observed_target,
            observed.then(|| serde_json::from_value(moved.clone()).unwrap())
        );
        if let Some(detail) = detail {
            assert_eq!(refusal.detail, detail);
        }
        assert_eq!(holds(&h, r.context.operation_id).await, 0, "{reason:?}");
    }

    let check = PublicationCheck {
        h: &h,
        svc: &svc,
        github: &github,
        policy: &main,
    };
    check.assert_local_refusal(receipt).await;

    // Pull-request mode refuses explicitly and never degrades to fast-forward.
    let pr = policy(PublicationMode::PullRequest, "refs/heads/main");
    let r = deliver_request(&h, candidate_id);
    let outcome = svc.deliver(&pr, r.clone(), &github).await.unwrap();
    assert!(
        matches!(&outcome, DeliveryOutcome::Refused(r) if r.reason == RefusalReason::PullRequestUnsupported)
    );
    let frozen: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_intents WHERE operation_id=$1")
            .bind(r.context.operation_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(frozen, 0, "pull-request refusal never freezes a publish");
    assert_eq!(*github.calls.lock().unwrap(), Vec::<&str>::new());

    // recovery_blocked is a typed state; the hold stays for operator recovery.
    let r = deliver_request(&h, candidate_id);
    let task = response_peer(&h, json!({"ok":false,"error":{"code":"recovery_blocked","message":"previous owner incarnation has no cleanup evidence","details":{},"retry":"manual"}})).await;
    let outcome = svc.deliver(&main, r.clone(), &github).await.unwrap();
    task.await.unwrap();
    unbind(&h);
    let DeliveryOutcome::RecoveryBlocked(blocked) = outcome else {
        panic!("expected recovery_blocked, got {outcome:?}");
    };
    assert_eq!(blocked.operation_id, r.context.operation_id);
    assert!(blocked.message.contains("cleanup evidence"));
    assert_eq!(holds(&h, r.context.operation_id).await, 1);
}

struct PublicationCheck<'a> {
    h: &'a Harness,
    svc: &'a DeliveryService,
    policy: &'a PublicationPolicy,
    github: &'a FakeGitHub,
}
impl PublicationCheck<'_> {
    async fn assert_local_refusal(&self, receipt: Uuid) {
        // A locally conflicted candidate is refused without any aq-git call.
        let conflicted = candidate(self.h, receipt);
        let mut result = ready(self.h, conflicted.context.operation_id);
        result["state"] = json!("conflicted");
        result["candidate"] = Value::Null;
        result["conflict_step"] = json!(0);
        let task = peer(self.h, result).await;
        self.svc.create_candidate(conflicted.clone()).await.unwrap();
        task.await.unwrap();
        unbind(self.h);
        let outcome = self
            .svc
            .deliver(
                self.policy,
                deliver_request(self.h, conflicted.context.operation_id),
                self.github,
            )
            .await
            .unwrap();
        assert!(
            matches!(&outcome, DeliveryOutcome::Refused(r) if r.reason == RefusalReason::Conflicted)
        );
    }
    async fn reconcile(&self, original: DeliverRequest) {
        let h = self.h;
        let svc = self.svc;
        let op = original.context.operation_id;
        let candidate_id = original.candidate_id;
        let expected_generation = generation(h, svc, op).await;
        assert_eq!(expected_generation, 4);
        let reconcile = |context| ReconcileRequest {
            context,
            target_operation_id: op,
            expected_generation,
        };
        let mut own = reconcile(context(h));
        own.context.operation_id = op;
        assert!(matches!(
            svc.reconcile(own).await,
            Err(DeliveryError::InvalidInput(_))
        ));

        // An ambiguous observation keeps the ambiguity and the hold.
        let first = reconcile(context(h));
        let task = peer(h, json!({"target_operation_id":op,"outcome":"indeterminate","observation_error":"remote_unavailable","historical_remote_effect":"unknown"})).await;
        let receipt = svc.reconcile(first.clone()).await.unwrap();
        let sent = task.await.unwrap();
        unbind(h);
        assert_eq!(sent["operation"], "operation.reconcile");
        assert_ne!(sent["operation_id"], json!(op));
        assert_eq!(
            sent["params"],
            json!({"target_operation_id":op,"expected_generation":4})
        );
        assert!(!receipt.outcome.unwrap().is_settled());
        assert_eq!(holds(h, op).await, 1);
        assert!(matches!(
            svc.deliver(self.policy, deliver_request(h, candidate_id), self.github)
                .await
                .unwrap(),
            DeliveryOutcome::DestinationBlocked
        ));

        // Presence is recorded as observed presence under a new reconcile ID.
        let second = reconcile(context(h));
        let task = peer(h, json!({"target_operation_id":op,"resolution":{"outcome":"already_present","observed_target":h.manifest.checkpoint,"observed_at":"2026-11-04T00:00:00Z","caused_by_this_operation":false}})).await;
        let receipt = svc.reconcile(second.clone()).await.unwrap();
        task.await.unwrap();
        unbind(h);
        let settled = receipt.outcome.as_ref().unwrap();
        assert!(settled.is_settled());
        assert!(matches!(
            &settled.resolution,
            Some(Resolution::AlreadyPresent { observed_target, .. }) if *observed_target == h.manifest.checkpoint
        ));
        assert_eq!(holds(h, op).await, 0);
        // Exact replay is the stored receipt, not a new observation.
        assert_eq!(svc.reconcile(second).await.unwrap().raw, receipt.raw);
        // The original operation ID stays fenced forever and is never resent.
        assert!(matches!(
            svc.deliver(self.policy, original, self.github)
                .await
                .unwrap(),
            DeliveryOutcome::Ambiguous { .. }
        ));
    }
}
async fn assert_owner_resolution(
    h: &Harness,
    svc: &DeliveryService,
    github: &FakeGitHub,
    candidate_id: Uuid,
) {
    // Explicit owner resolution requires the stopped-publisher attestation.
    let release = policy(PublicationMode::FastForward, "refs/heads/release");
    let stuck = deliver_request(h, candidate_id);
    let stuck_op = stuck.context.operation_id;
    let task = response_peer(h, indeterminate(stuck_op, "refs/heads/release")).await;
    svc.deliver(&release, stuck, github).await.unwrap();
    task.await.unwrap();
    unbind(h);
    let resolve = |old_publishers_stopped| ResolveRequest {
        context: context(h),
        target_operation_id: stuck_op,
        expected_generation: 4,
        disposition: ResolveDisposition::PermitNewOperation,
        reason: "remote inspected by owner".into(),
        old_publishers_stopped,
        evidence: json!({"inspection":"git ls-remote showed base"}),
    };
    assert!(matches!(
        svc.resolve(resolve(false)).await,
        Err(DeliveryError::InvalidInput(_))
    ));
    let task = peer(h, json!({"target_operation_id":stuck_op,"resolution":{"outcome":"resolved","disposition":"permit_new_operation","reason":"remote inspected by owner","evidence":{"inspection":"git ls-remote showed base"},"resolved_at":"2026-11-04T00:00:02Z","historical_remote_effect":"unknown"}})).await;
    let resolved = svc.resolve(resolve(true)).await.unwrap();
    let sent = task.await.unwrap();
    unbind(h);
    assert_eq!(sent["operation"], "operation.resolve");
    assert_eq!(sent["params"]["old_publishers_stopped"], true);
    assert!(resolved.outcome.unwrap().is_settled());
    assert_eq!(holds(h, stuck_op).await, 0);
}

fn refusal_cases(moved: &Value) -> [(Value, RefusalReason, bool, Option<&'static str>); 5] {
    let failed = |code: &str| json!({"ok":false,"operation_state":"failed","error":{"code":code,"message":code,"details":{},"retry":"refresh_inputs"}});
    [
        (
            failed("target_moved"),
            RefusalReason::TargetMoved,
            false,
            None,
        ),
        (
            json!({"ok":true,"result":{"outcome":"rejected","reason":"target_moved","observed_target":moved,"observed_at":"2026-11-04T00:00:00Z"}}),
            RefusalReason::TargetMoved,
            true,
            None,
        ),
        (failed("conflicted"), RefusalReason::Conflicted, false, None),
        (
            json!({"ok":true,"result":{"outcome":"rejected","reason":"remote_rejected","observed_at":"2026-11-04T00:00:00Z"}}),
            RefusalReason::Rejected,
            false,
            Some("remote_rejected"),
        ),
        (
            failed("unsupported_feature"),
            RefusalReason::TargetNotAllowed,
            false,
            None,
        ),
    ]
}
