//! Spec 0007 S7 end to end (live acceptance D5, review I8): a predecessor that
//! was handed off with a recorded result and a retained checkpoint; the next
//! poll offers its successor from the handoff commit, and the successor is
//! accepted and granted. The actual mounted poll, export, publication and
//! grant owners run; aq-git and the host are the synthetic native peers.
use crate::common::{
    native_offer::{self, Case, HANDOFF_CANDIDATE},
    native_started,
};
use aq_admission::{
    Label,
    control::{ControlReply, InitialGrantDelivery, decode_reply},
    startup::NativeOffer,
    transport::{Feature, ProcessOutcome, ResultReport},
};
use aq_server::{
    execution::{
        authority::hot::HotState,
        profiled::{Effects, Retention},
    },
    profiled_execution::{
        handoff, initial_grant, registration,
        replacement::{self, Assessment},
        results, started, stopped,
    },
    worker_api::{self, AuthenticatedNode},
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn auth(c: &Case) -> AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    let bearer = format!("Bearer {}", c.env.env.credential);
    headers.insert("authorization", bearer.parse().unwrap());
    worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap()
}
async fn granted(c: &mut Case, offer: &NativeOffer) -> InitialGrantDelivery {
    let (_, _, request, _) = native_offer::prepare_request(c, offer).await;
    let auth = auth(c).await;
    initial_grant::prepare(&c.env.env.state, &auth, &request, Uuid::new_v4())
        .await
        .unwrap()
}
fn sole_offer(body: &Value) -> NativeOffer {
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    page.offers.remove(0)
}

/// The granted predecessor starts, saves `checkpoint`, is stopped by its owner
/// (an `owner_stop` handoff) and reports Failed with that checkpoint's receipt.
async fn run_and_hand_off(c: &Case, grant: &InitialGrantDelivery, checkpoint: &str) {
    let state = &c.env.env.state;
    let auth = auth(c).await;
    let g = &grant.original;
    let mut start: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    let runtime = &state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .qualified_runtimes[0];
    start.operation_id = Uuid::new_v4();
    start.binding = g.grant.binding.clone();
    start.grant_id = g.grant.grant_id;
    (start.guardian_id, start.launch_id) = (g.guardian_id, g.launch_id);
    start.reservation_id = g.reservation_id;
    start.runtime = runtime.runtime.clone();
    start.qualification = runtime.qualification.clone();
    start.journal_revision = 3;
    let intent = &mut start.observation.guardian.intent;
    intent.launch_id = g.launch_id;
    intent.session = start.binding.session.clone();
    intent.manifest_digest = start.binding.launch_manifest_digest.clone();
    start.observation_digest = start.observation.digest().unwrap();
    started::record(state, &auth, &start).await.unwrap();
    let stop = native_started::stop_request(&start);
    // The legacy stop and result owners need host stop evidence: the synthetic
    // host advertises it for those reports, then the registration is restored.
    let pool = &c.env.env.pool;
    let node = "SELECT state->'native' FROM execution_nodes WHERE node_id=$1";
    let native: Value = sqlx::query_scalar(node)
        .bind(c.request.identity.node_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let mut stop_capable: registration::NativeRegistration =
        serde_json::from_value(native.clone()).unwrap();
    stop_capable
        .request
        .features
        .push(Feature::HostStopEvidenceV1);
    stop_capable.semantic_digest = registration::semantic_digest(&stop_capable.request).unwrap();
    let registered = |value: Value| async move {
        let mut tx = pool.begin().await.unwrap();
        let set =
            "UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1";
        sqlx::query("SET LOCAL session_replication_role=replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(set)
            .bind(c.request.identity.node_id)
            .bind(value)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    };
    registered(json!(stop_capable)).await;
    stopped::record(state, &auth, &stop).await.unwrap();

    // The worker's checkpoint transfer, centrally retained (validation only).
    let binding = &start.binding;
    let aq_session_protocol::SessionIdentity::Remote {
        node_id,
        session_id,
        ..
    } = binding.session
    else {
        unreachable!()
    };
    let (source, transfer_id, artifact_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let repository = c.env.frozen.project.binding.repository_id;
    let sha1 = |hex: &str| json!({"algorithm":"sha1","hex":hex});
    let manifest = json!({"transfer_id":transfer_id,"role":"checkpoint","checkpoint":sha1(checkpoint),
        "attempt":{"attempt_id":binding.attempt_id,"session_id":session_id,"generation":binding.assignment_generation},
        "resolved_spec_digest":binding.execution_spec_digest,
        "source":{"repository_id":repository,"binding_generation":1,"logical_identity":"synthetic-native-repository","base":sha1(&"a".repeat(40))},
        "artifact_id":artifact_id,"manifest_digest":"a".repeat(64),"bundle_digest":null,"manifest_bytes":1,"bundle_bytes":0});
    let receipt = json!({"receipt_id":source,"transfer_id":transfer_id,"repository_id":repository,"binding_generation":1,
        "source":sha1(checkpoint),"retention_handle":artifact_id,"manifest_digest":"b".repeat(64)});
    let central = json!({"repository_id":repository,"binding_generation":1,"transfer_id":artifact_id,"retention_handle":artifact_id,
        "manifest_digest":"b".repeat(64),"source":sha1(checkpoint),"retention_state":"active","historical":false,"receiver_installation_id":Uuid::new_v4()});
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt,central_receipt) VALUES($1,$2,$3,$4,$5,$6,$7,'retained',$8,$9)")
        .bind(transfer_id).bind(binding.attempt_id).bind(node_id).bind(repository).bind(artifact_id).bind(source)
        .bind(manifest).bind(receipt).bind(central).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
        .bind(binding.attempt_id).bind(node_id).execute(pool).await.unwrap();
    let report = ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: binding.clone(),
        supersedes: None,
        outcome: ProcessOutcome::Failed,
        stop: stop.evidence,
        source_receipt_ids: vec![source],
        pending_effect_ids: vec![],
        conclusion: Label::new("Handoff (owner stop): step1 and step2 saved").unwrap(),
    };
    results::record(state, &auth, &report).await.unwrap();
    registered(native).await;

    // Synthetic retained and quiescent owner facts; the actual stop is kept.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(binding.attempt_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let mut hot: HotState = serde_json::from_value(hot).unwrap();
    let artifact = |id: &str| crate::common::profiled::artifact(id, id.as_bytes());
    hot.retention = Retention::Retained {
        receipt: artifact("synthetic-retained"),
    };
    hot.effects = Effects::Quiescent {
        receipt: artifact("synthetic-effects"),
    };
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(binding.attempt_id)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let task = c.env.lineage.task_id;
    let now = (state.clock)().timestamp_millis() as u64;
    let assessed = replacement::assess_task(state, task, now).await.unwrap();
    let Assessment::Replace(r) = assessed else {
        panic!("the handed-off attempt is replaceable: {assessed:?}");
    };
    assert_eq!(r.exempt_attempts, 1, "an owner stop is not charged");
    replacement::release(state, task, &r).await.unwrap();
}

async fn export_of(c: &Case, attempt: Uuid) -> (Uuid, Value) {
    sqlx::query_as::<_, (Uuid, Value)>("SELECT x.operation_id,convert_from(e.request_bytes,'UTF8')::jsonb FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) WHERE x.attempt_id=$1")
        .bind(attempt).fetch_one(&c.env.env.pool).await.unwrap()
}

async fn handed_off() -> (Case, Uuid) {
    let (mut c, first) = native_offer::published().await;
    let grant = granted(&mut c, &first).await;
    run_and_hand_off(&c, &grant, &"5".repeat(40)).await;
    (c, first.offered.offer.binding.attempt_id)
}

#[tokio::test]
async fn handoff_successor_is_offered_and_granted_from_the_handoff_commit() {
    let (mut c, predecessor) = handed_off().await;

    // D5: this poll answered 409 on every retry in the live run.
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "the handoff successor is offered: {body}");
    let successor = sole_offer(&body);
    let attempt = successor.offered.offer.binding.attempt_id;
    assert_ne!(attempt, predecessor);
    assert_eq!(
        successor.offered.offer.manifest.bindings.task_id,
        c.env.lineage.task_id
    );
    let pool = &c.env.env.pool.clone();
    let row = sqlx::query_as::<_, (Option<Uuid>, Option<String>, Option<Uuid>)>("SELECT candidate_operation_id,candidate_oid,note_comment_id FROM attempt_handoffs WHERE attempt_id=$1")
        .bind(predecessor).fetch_one(pool).await.unwrap();
    let candidate = handoff::candidate_operation(predecessor);
    assert_eq!(row.0, Some(candidate));
    assert_eq!(row.1.as_deref(), Some(HANDOFF_CANDIDATE));
    assert!(row.2.is_some(), "the handoff note is posted");
    // A fresh export of the handoff commit, held by the candidate handle.
    let (old_op, _) = export_of(&c, predecessor).await;
    let (new_op, request) = export_of(&c, attempt).await;
    assert_ne!(new_op, old_op, "never the predecessor's export");
    assert_eq!(request["base"]["hex"], HANDOFF_CANDIDATE, "{request}");
    assert_eq!(request["source_retention_handle"], json!(candidate));
    let base: Value = sqlx::query_scalar("SELECT manifest FROM source_bases WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(base["source"]["base"]["hex"], HANDOFF_CANDIDATE, "{base}");

    // The successor is accepted and granted from that commit.
    let delivery = granted(&mut c, &successor).await;
    assert_eq!(delivery.original.grant.binding.attempt_id, attempt);
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(hot["status"], "granted", "{hot}");
    let comments: i64 =
        sqlx::query_scalar("SELECT count(*) FROM task_comments WHERE comment_id=$1")
            .bind(row.2)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(comments, 1, "one handoff note");
}

/// The source export guard stays exact: a successor may export only the
/// commit recorded as its predecessor's handoff candidate. A refusal skips the
/// task for this poll (Q1); it never fails the poll.
#[tokio::test]
async fn handoff_successor_cannot_export_a_commit_other_than_the_recorded_candidate() {
    let (c, predecessor) = handed_off().await;
    let pool = &c.env.env.pool;
    sqlx::query("UPDATE attempt_handoffs SET candidate_operation_id=$2,candidate_oid=$3 WHERE attempt_id=$1")
        .bind(predecessor).bind(handoff::candidate_operation(predecessor)).bind("7".repeat(40))
        .execute(pool).await.unwrap();
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    let exports = count("SELECT count(*) FROM profiled_source_exports").await;
    for _ in 0..2 {
        let (status, body) = native_offer::poll(&c).await;
        assert_eq!(
            status, 200,
            "a refused handoff never fails the poll: {body}"
        );
        let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
        else {
            panic!("{body}")
        };
        assert!(page.offers.is_empty(), "{body}");
    }
    assert_eq!(
        count("SELECT count(*) FROM profiled_source_exports").await,
        exports,
        "no export of an unrecorded commit is reserved"
    );
    assert_eq!(count("SELECT count(*) FROM execution_attempts").await, 1);
}

/// #116 I1, M1: a handoff successor is exempt from the lineage budgets, so a refused one would be
/// skipped forever. After `STUCK_SKIPS` consecutive refused polls (and not before) the task is
/// held visibly as `handoff_successor_stuck`: recorded once, one system comment, its work view.
/// AQ keeps trying it.
#[tokio::test]
async fn a_handoff_successor_refused_on_every_poll_is_held_after_stuck_skips_not_before() {
    use aq_machine_control::holds;
    let (c, predecessor) = handed_off().await;
    let pool = &c.env.env.pool;
    sqlx::query("UPDATE attempt_handoffs SET candidate_operation_id=$2,candidate_oid=$3 WHERE attempt_id=$1")
        .bind(predecessor).bind(handoff::candidate_operation(predecessor)).bind("7".repeat(40))
        .execute(pool).await.unwrap();
    let task = c.env.lineage.task_id;
    let hold_comment = Uuid::new_v5(&task, b"aq.task-hold/1");
    let skips = || async {
        sqlx::query_scalar::<_, i32>(
            "SELECT skips FROM successor_skips WHERE task_id=$1 AND predecessor=$2",
        )
        .bind(task)
        .bind(predecessor)
        .fetch_optional(pool)
        .await
        .unwrap()
    };
    let posted = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM task_comments WHERE comment_id=$1")
            .bind(hold_comment)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    for n in 1..=holds::STUCK_SKIPS {
        let (status, body) = native_offer::poll(&c).await;
        assert_eq!(status, 200, "poll {n}: {body}");
        let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
        else {
            panic!("{body}")
        };
        assert!(page.offers.is_empty(), "{body}");
        assert_eq!(skips().await, Some(n));
        if n < holds::STUCK_SKIPS {
            assert!(
                super::holds::hold_rows(&c, task).await.is_empty(),
                "not held after {n} skips"
            );
            assert_eq!(posted().await, 0);
        }
    }
    let readiness = super::holds::work(&c, task).await;
    assert_eq!(readiness["state"], "held", "{readiness}");
    let reasons = readiness["reasons"].to_string();
    assert!(
        reasons.contains("\"hold_reason\":\"handoff_successor_stuck\""),
        "{readiness}"
    );
    assert_eq!(
        super::holds::hold_rows(&c, task).await,
        [("handoff_successor_stuck".to_owned(), hold_comment, true)]
    );
    // AQ keeps trying it; the hold and its one comment stay as they are.
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(skips().await, Some(holds::STUCK_SKIPS + 1));
    assert_eq!(posted().await, 1);
    let comments = super::holds::system_comments(&c, task).await;
    let held: Vec<_> = comments
        .iter()
        .filter(|(id, _)| *id == hold_comment)
        .collect();
    assert_eq!(held.len(), 1, "{comments:?}");
    assert!(
        held[0]
            .1
            .contains("its handoff successor was refused on 20 polls in a row"),
        "{held:?}"
    );
}
