//! G10 owner operations read over actual H3/H4 rows for one profiled session.
use super::waits::{registered, wait_env};
use super::*;
use aq_server::{
    dashboard_api::contract::SessionOperations,
    stall::{evidence::Explanation, ladder, policy::ExecutionPolicy},
};
use aq_wait_engine::wire;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn read(app: &axum::Router, path: &str, email: &str) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn owner_operations_read_projects_stall_wait_nudge_progress_and_manifest() {
    let w = Box::pin(wait_env()).await;
    let f = &w.env.f;
    let pool = f.db.pool().await;
    let attempt = w.attempt();
    let wait_id = Uuid::new_v4();
    let request = wire::Request::seal(
        Uuid::new_v4(),
        wire::Command::RegisterWait(w.registration(wait_id, w.condition().await)),
    )
    .unwrap();
    registered(&w.register(&request).await);
    let (session, task): (Uuid, Uuid) =
        sqlx::query_as("SELECT session_id,task_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();

    // H3: a quarantined episode explained by a wait, and two nudges.
    let mut ep = ladder::Episode::open(attempt, task, &ExecutionPolicy::v1(), NOW);
    ep.rung = ladder::Rung::Quarantined;
    ep.rung_since_ms = NOW + 10;
    ep.explanation = Some(Explanation::RegisteredWait);
    ep.nudges = 2;
    ep.notified = true;
    sqlx::query("INSERT INTO stall_episodes(episode_id,attempt_id,lineage_id,policy_version,revision,rung,state,updated_at_ms) VALUES($1,$2,$3,1,7,'quarantined',$4,$5)")
        .bind(ep.episode_id).bind(attempt).bind(task).bind(json!(ep)).bind((NOW + 20) as i64)
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'replacement_ceiling','owner','{}',$3,$4)")
        .bind(attempt).bind(task).bind(Uuid::new_v4()).bind((NOW + 30) as i64)
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key,next_cursor) VALUES('g10-ops',3)")
        .execute(&pool)
        .await
        .unwrap();
    let (handed, inbox) = (Uuid::new_v4(), Uuid::new_v4());
    for (cursor, id, inbox_only, owner) in [(1i64, handed, false, true), (2, inbox, true, false)] {
        sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,'g10-ops',$2,$1,'{}','{}','x','nudge',$3)")
            .bind(id).bind(cursor).bind(NOW as i64).execute(&pool).await.unwrap();
        let action = if owner {
            None
        } else {
            let action = Uuid::new_v4();
            sqlx::query("INSERT INTO stall_actions(action_id,episode_id,kind,ordinal,expected_revision,intent,created_at_ms,receipt,receipted_at_ms,delivery) VALUES($1,$2,'nudge',1,1,'{}',$3,'{}',$3,'deferred')")
                .bind(action).bind(ep.episode_id).bind(NOW as i64).execute(&pool).await.unwrap();
            Some(action)
        };
        sqlx::query("INSERT INTO stall_nudges(message_id,attempt_id,action_id,inbox_only,created_at_ms) VALUES($1,$2,$3,$4,$5)")
            .bind(id).bind(attempt).bind(action).bind(inbox_only).bind((NOW + cursor as u64) as i64)
            .execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO mailbox_deliveries(delivery_id,message_id,attempt_id,disposition,updated_at_ms) VALUES($1,$2,$3,'handed_to_harness',$4)")
        .bind(Uuid::new_v4()).bind(handed).bind(attempt).bind((NOW + 40) as i64)
        .execute(&pool).await.unwrap();
    // Semantic progress counts tool starts; terminal output does not.
    sqlx::query("INSERT INTO observation_nodes(node_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    for (seq, at, kind) in [
        (900_001i64, NOW + 50, "tool_started"),
        (900_002, NOW + 60, "usage"),
    ] {
        sqlx::query("INSERT INTO observation_events(event_id,node_id,sequence,lineage_id,attempt_id,recorded_at_ms,record,decision) VALUES($1,$2,$3,$4,$5,$6,$7,'{}')")
            .bind(Uuid::new_v4()).bind(f.node.node_id).bind(seq).bind(task).bind(attempt).bind(at as i64)
            .bind(json!({"body":{"kind":{"kind":kind}}}))
            .execute(&pool).await.unwrap();
    }

    let app = aq_server::build_admin_router(f.state.clone());
    let (code, boot) = read(&app, "/api/v1/bootstrap", "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{boot}");
    for name in ["waits.read", "session.nudge", "session.operations.read"] {
        let cap = boot["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(cap["enabled"], true, "{name}");
    }
    let path = format!("/api/v1/sessions/{session}/operations");
    let (code, body) = read(&app, &path, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let ops: SessionOperations = serde_json::from_value(body["data"].clone()).unwrap();
    assert_eq!((ops.session_id, ops.attempt_id), (session, attempt));
    let stall = ops.stall.unwrap();
    assert_eq!((stall.rung.as_str(), stall.revision), ("quarantined", 7));
    assert_eq!(stall.explanation.as_deref(), Some("registered_wait"));
    assert_eq!((stall.nudges, stall.notified), (2, true));
    let q = stall.quarantine.unwrap();
    assert_eq!(
        (q.scope.as_str(), q.lineage_id, q.reason.as_str()),
        ("session_and_lineage", task, "replacement_ceiling")
    );
    assert_eq!(
        ops.semantic_progress.unwrap().observed_at.0,
        "1970-01-01T00:16:40.050Z"
    );
    let wait = ops.wait.unwrap();
    assert_eq!((wait.id, wait.phase.as_str()), (wait_id, "pending"));
    assert!(wait.deadline.is_some());
    let nudges: Vec<_> = ops
        .nudges
        .iter()
        .map(|n| (n.message_id, n.source.as_str(), n.delivery.as_str()))
        .collect();
    assert_eq!(
        nudges,
        [
            (inbox, "stall_ladder", "inbox_only"),
            (handed, "owner", "delivered")
        ]
    );
    assert_eq!(
        ops.nudges[1].delivered_at.as_ref().unwrap().0,
        "1970-01-01T00:16:40.040Z"
    );
    let instructions = ops.instructions.unwrap();
    assert!(instructions.catalog.digest.0.starts_with("sha256:"));
    assert!(ops.requested_permissions.is_some() && ops.effective_permissions.is_some());
    assert!(!body["data"].to_string().contains("credential"), "{body}");

    // Owner-only: a viewer is refused.
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'x@localhost','viewer','dev','x@localhost',now())")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let (code, _) = read(&app, &path, "x@localhost").await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, boot) = read(&app, "/api/v1/bootstrap", "x@localhost").await;
    assert_eq!(code, StatusCode::OK);
    let enabled = |name: &str| {
        boot["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap()["enabled"]
            .clone()
    };
    assert_eq!(enabled("session.operations.read"), false);
    assert_eq!(enabled("waits.read"), false);
}
