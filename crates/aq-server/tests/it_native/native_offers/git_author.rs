use crate::common::native_offer::{Case, assembled, poll_offered, prepare_request, setup};
use aq_admission::control::{ControlReply, decode_reply};
use aq_profiled_state::git_author::{GitAuthor, HEADER, PollReply, VERSION};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;

fn author() -> GitAuthor {
    GitAuthor {
        name: "Alice+Work".into(),
        email: "Alice+Work@example.test".into(),
    }
}

async fn snapshot(case: &Case, project: Uuid, workspace: Uuid, task: Uuid, confirmed: bool) {
    let author = author();
    sqlx::query(
        "INSERT INTO task_git_authors(source,operation_id,project_id,workspace_id,task_id,
         requester_id,name,email,confirmed) VALUES('human_graph',$1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(Uuid::new_v4())
    .bind(project)
    .bind(workspace)
    .bind(task)
    .bind(case.env.actor)
    .bind(author.name)
    .bind(author.email)
    .bind(confirmed)
    .execute(&case.env.env.pool)
    .await
    .unwrap();
}

async fn snapshot_confirmed_author(case: &Case) {
    let lineage = &case.env.lineage;
    snapshot(
        case,
        lineage.project_id,
        lineage.workspace_id,
        lineage.task_id,
        true,
    )
    .await;
}

async fn poll(case: &Case, headers: &[&str]) -> Value {
    let mut req = axum::http::Request::post("/v1/worker/profiled/poll")
        .header(
            "authorization",
            format!("Bearer {}", case.env.env.credential),
        )
        .header("content-type", "application/json");
    for value in headers {
        req = req.header(HEADER, *value);
    }
    let reply = aq_server::build_worker_router(case.env.env.state.clone())
        .oneshot(
            req.body(axum::body::Body::from(
                serde_json::to_vec(&case.request).unwrap(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    let status = reply.status();
    let bytes = reply.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}

async fn offer(case: &Case, headers: &[&str]) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let reply = poll(case, headers).await;
        let offers = reply["body"]["offers"].as_array().unwrap();
        if !offers.is_empty() {
            assert_eq!(offers.len(), 1);
            let PollReply::Poll(page) = serde_json::from_value(reply.clone()).unwrap();
            page.validate_for(&case.request).unwrap();
            return reply;
        }
        assert!(tokio::time::Instant::now() < deadline, "{reply}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn assert_no_attempt(case: &Case) {
    assembled(case).await;
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}

async fn assert_unsupported_workers_receive_no_offer(case: &Case) {
    for unsupported in [&[][..], &["2"], &[VERSION, VERSION]] {
        let reply = poll(case, unsupported).await;
        let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&reply).unwrap()).unwrap()
        else {
            panic!("{reply}")
        };
        assert!(page.offers.is_empty());
    }
}

#[tokio::test]
async fn negotiated_author_is_sealed_and_replayed_from_the_attempt() {
    let mut case = setup().await;
    let lineage = &case.env.lineage;
    snapshot_confirmed_author(&case).await;
    let reply = offer(&case, &[VERSION]).await;
    let PollReply::Poll(mut page) = serde_json::from_value(reply.clone()).unwrap();
    let delivered = page.offers.remove(0);
    let binding = &delivered.native.offered.offer.binding;
    assert_eq!(delivered.git_author.as_ref(), Some(&author()));
    // This is the real closed schema shipped to workers that lack support.
    assert!(decode_reply(&serde_json::to_vec(&reply).unwrap()).is_err());
    let original: Value = sqlx::query_scalar(
        "SELECT graph_task_observation FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(binding.attempt_id)
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(original["native_offer"]["git_author"], json!(author()));

    // Perturb the upstream fixture on the isolated database. Any re-read
    // instead of replay would now drop the author.
    sqlx::query("DELETE FROM task_git_authors WHERE task_id=$1")
        .bind(lineage.task_id)
        .execute(&case.env.env.pool)
        .await
        .unwrap();
    assert_unsupported_workers_receive_no_offer(&case).await;
    let reply = offer(&case, &[VERSION]).await;
    let PollReply::Poll(page) = serde_json::from_value(reply).unwrap();
    let replayed = &page.offers[0];
    assert_eq!(replayed.git_author, delivered.git_author);
    assert_eq!(&replayed.native.offered.offer.binding, binding);
    let (_, plan, prepare, _) = prepare_request(&mut case, &replayed.native).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", case.env.env.credential)
            .parse()
            .unwrap(),
    );
    let state = &case.env.env.state;
    let auth = aq_server::worker_api::authenticate_context(state, &headers)
        .await
        .unwrap();
    let nonce = Uuid::new_v4();
    let grant =
        aq_server::profiled_execution::initial_grant::prepare(state, &auth, &prepare, nonce)
            .await
            .unwrap();
    grant
        .validate_for(&prepare, &plan, &replayed.native, nonce)
        .unwrap();
    assert_eq!(&grant.original.grant.binding, binding);
}

#[tokio::test]
async fn legacy_offer_keeps_its_closed_shape_when_reoffered_to_new_worker() {
    let case = setup().await;
    let (status, legacy) = poll_offered(&case).await;
    assert_eq!(status, 200, "{legacy}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&legacy).unwrap()).unwrap()
    else {
        panic!("{legacy}")
    };
    assert_eq!(page.offers.len(), 1);
    assert!(legacy["body"]["offers"][0].get("git_author").is_none());
    snapshot_confirmed_author(&case).await;
    let replay = offer(&case, &[VERSION]).await;
    assert!(replay["body"]["offers"][0].get("git_author").is_none());
    assert_eq!(
        legacy["body"]["offers"][0]["offered"]["offer"]["binding"],
        replay["body"]["offers"][0]["offered"]["offer"]["binding"]
    );
}

#[tokio::test]
async fn legacy_worker_leaves_attributed_task_available_for_capable_poll() {
    let case = setup().await;
    let lineage = &case.env.lineage;
    snapshot_confirmed_author(&case).await;
    let reply = poll(&case, &[]).await;
    assert_eq!(reply["body"]["offers"], json!([]));
    assert_no_attempt(&case).await;

    let reply = offer(&case, &[VERSION]).await;
    let choice = &reply["body"]["offers"][0];
    assert_eq!(
        choice["offered"]["offer"]["manifest"]["bindings"]["task_id"],
        json!(lineage.task_id)
    );
    assert_eq!(choice["git_author"], json!(author()));
}

#[tokio::test]
async fn legacy_worker_can_take_unattributed_task_behind_attributed_task() {
    let case = setup().await;
    snapshot_confirmed_author(&case).await;
    case.mode.store(7, Ordering::SeqCst);
    let (status, reply) = poll_offered(&case).await;
    assert_eq!(status, 200, "{reply}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&reply).unwrap()).unwrap()
    else {
        panic!("{reply}")
    };
    assert_eq!(page.offers.len(), 1);
    assert_eq!(
        page.offers[0].offered.offer.manifest.bindings.task_id,
        case.second_task
    );
    let attempted: Vec<Uuid> = sqlx::query_scalar("SELECT task_id FROM execution_attempts")
        .fetch_all(&case.env.env.pool)
        .await
        .unwrap();
    assert_eq!(attempted, vec![case.second_task]);
}

#[tokio::test]
async fn publication_rechecks_author_confirmed_after_candidate_selection() {
    let case = setup().await;
    case.mode.store(12, Ordering::SeqCst);
    let confirm = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            case.observation_entered.acquire(),
        )
        .await
        .unwrap()
        .unwrap()
        .forget();
        snapshot_confirmed_author(&case).await;
        case.mode.store(0, Ordering::SeqCst);
        case.observation_resume.add_permits(1);
    };
    let (reply, ()) = tokio::join!(poll(&case, &[]), confirm);
    assert_eq!(reply["body"]["offers"], json!([]));
    assert_no_attempt(&case).await;

    let reply = offer(&case, &[VERSION]).await;
    assert_eq!(reply["body"]["offers"][0]["git_author"], json!(author()));
}

#[tokio::test]
async fn malformed_legacy_author_leaves_a_valid_unauthored_offer() {
    let case = setup().await;
    let lineage = &case.env.lineage;
    snapshot_confirmed_author(&case).await;
    sqlx::query("UPDATE task_git_authors SET name=$1,email=$2 WHERE task_id=$3")
        .bind("Old<Account@")
        .bind("Old<Account@@example.test")
        .bind(lineage.task_id)
        .execute(&case.env.env.pool)
        .await
        .unwrap();
    let original = offer(&case, &[VERSION]).await;
    assert!(original["body"]["offers"][0].get("git_author").is_none());
    let legacy = offer(&case, &[]).await;
    assert!(legacy["body"]["offers"][0].get("git_author").is_none());
}

#[tokio::test]
async fn absent_author_ignores_other_scopes_and_unconfirmed_or_graph_input() {
    let case = setup().await;
    let lineage = &case.env.lineage;
    let (project, workspace, task) = (lineage.project_id, lineage.workspace_id, lineage.task_id);
    snapshot(&case, Uuid::new_v4(), workspace, task, true).await;
    snapshot(&case, project, Uuid::new_v4(), task, true).await;
    snapshot(&case, project, workspace, Uuid::new_v4(), true).await;
    snapshot(&case, project, workspace, task, false).await;
    *case.metadata.lock().unwrap() = json!({"git_author": author()});
    let original = offer(&case, &[VERSION]).await;
    assert!(original["body"]["offers"][0].get("git_author").is_none());

    snapshot(&case, project, workspace, task, true).await;
    let replay = offer(&case, &[VERSION]).await;
    assert!(replay["body"]["offers"][0].get("git_author").is_none());
    assert_eq!(
        original["body"]["offers"][0]["offered"]["offer"]["binding"],
        replay["body"]["offers"][0]["offered"]["offer"]["binding"]
    );
}
