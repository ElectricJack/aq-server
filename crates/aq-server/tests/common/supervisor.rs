//! Real supervisor generation credential and command request helpers for SQL fixtures.
use aq_server::AppState;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

/// Issue a real supervisor credential for a synthetic started turn.
pub async fn supervisor_token(
    state: &AppState,
    pool: &sqlx::PgPool,
    project: Option<Uuid>,
) -> String {
    use aq_central_authority as ca;
    use aq_central_authority::owner::{EvidenceVerifier, OwnerResult, PermitWindow, SessionOwner};
    use aq_server::agent_access::supervisor_conversation::{
        self as conversation, dispatch, reply,
        reservations::{
            Configure, LocalPolicy, PolicyKind, PreviewRequest, ReservationOwner, Reserve,
        },
    };

    struct Evidence;

    #[async_trait::async_trait]
    impl EvidenceVerifier for Evidence {
        async fn authorize_plan(
            &self,
            tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
            plan: &ca::CentralPlan,
        ) -> OwnerResult<()> {
            let digest: String = sqlx::query_scalar(
                "SELECT input_digest FROM central_supervisor_dispatches WHERE dispatch_id=$1",
            )
            .bind(plan.dispatch_id)
            .fetch_one(&mut **tx)
            .await?;
            let digest = serde_json::from_value(json!(digest)).unwrap();
            dispatch::authorize_dispatch(tx, plan, &digest)
                .await
                .map_err(|_| ca::owner::OwnerError::Conflict)
        }

        async fn verify_event(
            &self,
            _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
            _: &ca::Run,
            _: &ca::Event,
        ) -> OwnerResult<()> {
            Ok(())
        }
    }

    let now = u64::try_from((state.clock)().timestamp_millis()).unwrap();
    let user = state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let (conversation_id, actor_id): (Uuid, Uuid) = if let Some(project) = project {
        sqlx::query_as("UPDATE supervisor_controls c SET enabled=TRUE,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE c.conversation_id=i.conversation_id AND i.project_id=$1 RETURNING c.conversation_id,c.actor_id")
            .bind(project).bind(user.id).fetch_one(pool).await.unwrap()
    } else {
        let conversation = sqlx::query_scalar(
            "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        (conversation, Uuid::new_v4())
    };
    let owner = ReservationOwner::new(state);
    let configured = owner
        .configure_actor(
            &user,
            &serde_json::to_vec(&Configure {
                operation_id: Uuid::new_v4(),
                actor_id,
                expected_actor_revision: 0,
                policy: LocalPolicy {
                    schema_version: 1,
                    kind: PolicyKind::GlobalSupervisorReservation,
                    actor_id,
                    conversation_id,
                    enabled: true,
                },
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let note = json!({
        "operation_id": Uuid::new_v4(), "message_id": Uuid::new_v4(),
        "expected_conversation_revision": "1", "body": "Inspect the retained hold",
        "context": [], "attachments": []
    });
    let response = conversation::routes(state)
        .with_state(state.clone())
        .oneshot(
            Request::post(format!(
                "/api/human/conversations/{conversation_id}/messages"
            ))
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1")
            .header("content-type", "application/json")
            .body(Body::from(note.to_string()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let preview = owner
        .preview_batch(
            &user,
            PreviewRequest {
                actor_id,
                expected_actor_revision: configured.actor_revision,
                scan_after: 0,
                through_cursor: None,
            },
        )
        .await
        .unwrap();
    let reserved = owner
        .reserve_batch(
            &user,
            &serde_json::to_vec(&Reserve {
                operation_id: Uuid::new_v4(),
                reservation_id: Uuid::new_v4(),
                actor_id,
                expected_actor_revision: preview.actor_revision,
                expected_policy_revision: preview.policy_revision,
                scan_after: preview.scan_after,
                through_cursor: preview.through_cursor,
                selection_digest: preview.selection_digest,
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let reservation = reserved.reservation.unwrap().id;
    let examples: serde_json::Value = serde_json::from_str(include_str!(
        "../../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let ca::Document::Plan(mut plan) =
        ca::decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!("central plan example changed");
    };
    let ca::CentralSubject::CentralConversation {
        actor_id: actor,
        conversation_id: conversation,
        ..
    } = &mut plan.subject;
    (*actor, *conversation) = (actor_id, conversation_id);
    let ca::SessionIdentity::Central {
        session_id,
        generation,
        ..
    } = &mut plan.session
    else {
        panic!("expected a central session");
    };
    (*session_id, *generation, plan.generation) = (Uuid::new_v4(), 1, 1);
    let turn = dispatch::prepare_turn(pool, actor_id, reservation, 1, *session_id, now)
        .await
        .unwrap();
    (plan.batch, plan.dispatch_id, plan.host_operation_id) =
        (turn.batch, turn.dispatch_id, turn.host_operation_id);
    let sessions = SessionOwner::new(pool.clone(), Evidence);
    let clock = ca::Clock {
        machine_boot_id: Uuid::new_v4(),
        boot_ms: 1000,
    };
    sessions.prepare(plan.clone(), clock.clone()).await.unwrap();
    let binding = plan.binding().unwrap();
    let launch = ca::HostLaunchRef {
        binding: binding.clone(),
        launch_id: Uuid::new_v4(),
        intent: ca::ArtifactRef {
            id: ca::Text::new("synthetic-holder").unwrap(),
            version: ca::Text::new("1").unwrap(),
            digest: ca::Digest::of(b"synthetic-holder"),
        },
    };
    let started = sessions
        .start(&binding, launch, PermitWindow::new(100_000, clock))
        .await
        .unwrap();
    assert_eq!(started.decision, ca::Decision::Applied);
    reply::issue_credential(pool, actor_id, 60_000, now - 1)
        .await
        .unwrap()
}

pub async fn supervised(
    machine: &axum::Router,
    token: &str,
    command: &str,
    params: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = machine
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/commands/{command}"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"params":params}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
