//! Actual SQL result recording and mounted human route, no browser/API fixtures.
use super::*;
use aq_server::profiled_execution::human_results::{self, Page};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;
async fn get(f: &Fixture, path: &str) -> (StatusCode, serde_json::Value) {
    let response = human_results::routes()
        .with_state(f.state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn human_metadata_projects_exact_revisions_corrections_and_bounded_pages() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    correction.conclusion = Label::new("corrected retained claim").unwrap();
    results::record(&f.state, &auth, &correction).await.unwrap();
    let (status, page) = get(&f, "/api/human/results?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["coverage"], human_results::COVERAGE);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_after"].as_str().unwrap();
    let (_, next) = get(&f, &format!("/api/human/results?limit=1&after={cursor}")).await;
    assert!(next["next_after"].is_null());
    assert_ne!(page["items"][0]["report_id"], next["items"][0]["report_id"]);
    let path = format!(
        "/api/human/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let (status, detail) = get(&f, &path).await;
    assert_eq!(status, StatusCode::OK);
    let old = &detail["items"][0];
    assert_eq!(old["current_report_head"], false);
    assert_eq!(old["head_report_id"], json!(correction.report_id));
    assert_eq!(old["conclusion"], report.conclusion.as_str());
    assert_eq!(old["project_id"], json!(f.selection.lineage.project_id));
    assert_eq!(old["task_id"], json!(f.selection.lineage.task_id));
    assert!(old.get("origin").is_none(), "default v1 field set changed");
    let (status, mut v2) = get(&f, &format!("{path}?profile=recorded_native_v2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2["coverage"], human_results::NATIVE_V2_COVERAGE);
    assert_eq!(v2["items"][0]["origin"], "legacy_prepare_v1");
    v2["items"][0].as_object_mut().unwrap().remove("origin");
    v2["coverage"] = json!(human_results::COVERAGE);
    assert_eq!(
        serde_json::to_vec(&detail).unwrap(),
        serde_json::to_vec(&v2).unwrap(),
        "opt-in origin must not alter legacy metadata bytes/fields"
    );
    for forbidden in [
        "credential",
        "execution_spec",
        "launch_manifest",
        "accepted",
        "settled",
        "eligible",
        "stop",
        "identity",
    ] {
        assert!(old.get(forbidden).is_none(), "unexpected {forbidden}");
    }
    assert_eq!(
        get(
            &f,
            &format!(
                "/api/human/results/{}/revisions/{}",
                report.report_id,
                "f".repeat(64)
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, empty) = get(
        &f,
        &format!("/api/human/results?project_id={}", Uuid::new_v4()),
    )
    .await;
    assert!(empty["items"].as_array().unwrap().is_empty());
    assert!(
        human_results::read(
            &f.state,
            f.actor,
            &Page {
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
    assert_eq!(
        get(&f, "/api/human/results?limit=26").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&f, "/api/human/results?limit=0").await.0,
        StatusCode::BAD_REQUEST
    );
    if std::env::var("AQ_TEST_RETAIN_RESULTS_DEMO").as_deref() == Ok("1") {
        // Explicit isolated preview fixture, after assertions. No provider run.
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
        std::mem::forget(f.db.clone());
        eprintln!(
            "retained_results_demo_database={name} report_id={} revision={}",
            report.report_id,
            receipt.report_digest.as_str()
        );
    }
}
#[tokio::test]
async fn stopped_without_report_is_empty_and_current_viewer_cannot_read_results() {
    let (f, _, report) = prepare(true, true).await;
    let (_, page) = get(&f, "/api/human/results").await;
    assert!(page["items"].as_array().unwrap().is_empty());
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(get(&f, "/api/human/results").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&f, "/api/human/results?profile=recorded_native_v2")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        human_results::read(&f.state, f.actor, &Page::default(), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dashboard_search_uses_retained_result_metadata_and_digest() {
    let (f, _, mut report) = prepare(true, true).await;
    report.conclusion = Label::new("example result").unwrap();
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .oneshot(
            Request::get("/api/v1/search?q=example&types=result")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 262144).await.unwrap()).unwrap();
    let group = body["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["type"] == "result")
        .unwrap();
    assert_eq!(group["hits"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(group["hits"][0]["title"], "example result");
    assert_eq!(group["hits"][0]["ref"]["id"], json!(report.report_id));
    assert_eq!(
        group["hits"][0]["ref"]["revision"],
        format!("sha256:{}", receipt.report_digest.as_str())
    );
    assert_eq!(
        group["hits"][0]["project"]["id"],
        json!(f.selection.lineage.project_id)
    );
    for excluded in [
        "execution_spec",
        "launch_manifest",
        "credentials",
        "source_receipt_ids",
    ] {
        assert!(!body.to_string().contains(excluded));
    }
}
#[tokio::test]
async fn project_listing_isolates_an_unreadable_report_and_keeps_projects_apart() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    results::record(&f.state, &auth, &correction).await.unwrap();
    let project = f.selection.lineage.project_id;
    let page = human_results::read_project(&f.state, project, None, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(!page.incomplete && page.unavailable.is_empty());
    // Another project sees nothing, and cannot probe an exact revision here.
    let other = Uuid::new_v4();
    let page = human_results::read_project(&f.state, other, None, None, None)
        .await
        .unwrap();
    assert!(page.items.is_empty() && !page.incomplete);
    assert!(
        human_results::read_project(
            &f.state,
            other,
            None,
            None,
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
    // The original's stored bytes stop being its canonical encoding, with the
    // row's digest kept equal to the new bytes (so the row stays valid for
    // `profiled_result_reports_check`): its validation refuses. The correction
    // still lists, the page says it is incomplete.
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE profiled_result_reports SET report_bytes=report_bytes||'\\x0a'::bytea, \
         report_digest=encode(sha256(report_bytes||'\\x0a'::bytea),'hex') WHERE report_id=$1",
    )
    .bind(report.report_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports ENABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page = human_results::read_project(&f.state, project, None, None, None)
        .await
        .unwrap();
    assert!(page.incomplete);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].report_id, correction.report_id);
    assert_eq!(page.unavailable.len(), 1);
    assert_eq!(page.unavailable[0].report_id, report.report_id);
    let (status, human) = get(&f, "/api/human/results").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(human["incomplete"], true);
    assert_eq!(human["items"].as_array().unwrap().len(), 1);
    // An exact read of the unreadable report still fails as a whole.
    assert!(
        human_results::read_project(
            &f.state,
            project,
            None,
            None,
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
}

/// Issue a real supervisor credential for a synthetic started turn.
async fn supervisor_token(f: &Fixture, project: Option<Uuid>) -> String {
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

    let pool = f.db.pool().await;
    let user = f.state.store.find_user(f.actor).await.unwrap().unwrap();
    let (conversation_id, actor_id): (Uuid, Uuid) = if let Some(project) = project {
        sqlx::query_as("UPDATE supervisor_controls c SET enabled=TRUE,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE c.conversation_id=i.conversation_id AND i.project_id=$1 RETURNING c.conversation_id,c.actor_id")
            .bind(project).bind(f.actor).fetch_one(&pool).await.unwrap()
    } else {
        let conversation = sqlx::query_scalar(
            "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        (conversation, Uuid::new_v4())
    };
    let owner = ReservationOwner::new(&f.state);
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
    let response = conversation::routes(&f.state)
        .with_state(f.state.clone())
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
        "../../../../aq-central-authority/artifacts/examples.json"
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
    let turn = dispatch::prepare_turn(&pool, actor_id, reservation, 1, *session_id, NOW)
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
    reply::issue_credential(&pool, actor_id, 60_000, NOW - 1)
        .await
        .unwrap()
}

async fn supervised(
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

#[tokio::test]
async fn a_supervisor_reads_only_the_results_its_scope_covers() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let (own, other) = (f.selection.lineage.project_id, Uuid::new_v4());
    sqlx::query(
        "INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')",
    )
    .bind(other)
    .bind(json!({"name":"Other","enabled":true}))
    .execute(&f.db.pool().await)
    .await
    .unwrap();
    #[cfg(not(feature = "supervisor_full_access"))]
    let machine = aq_server::build_worker_router(f.state.clone());
    #[cfg(feature = "supervisor_full_access")]
    let machine = aq_server::build_worker_router_with_owner_app(
        f.state.clone(),
        aq_server::build_admin_router(f.state.clone()),
    );
    let supervisor = supervisor_token(&f, Some(own)).await;
    let foreign = supervisor_token(&f, Some(other)).await;
    let global = supervisor_token(&f, None).await;
    let detail = json!({"result_id":report.report_id,"revision":receipt.report_digest.as_str()});
    let mut exact = detail.clone();
    exact["profile"] = json!("recorded_native_v2");
    let results = [("result.show", exact), ("result.detail", detail)];
    let attempt = json!({"attempt_id":report.binding.attempt_id});
    let refused = |(status, body): (StatusCode, serde_json::Value)| {
        assert_eq!(body["error"], "supervisor_refused", "{body}");
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    };
    for (token, has_task_assessment) in [(&supervisor, false), (&global, true)] {
        for (name, params) in &results {
            let (status, detail) = supervised(&machine, token, name, params.clone()).await;
            assert_eq!(status, StatusCode::OK, "{name}: {detail}");
            let evidence = if *name == "result.show" {
                assert_eq!(detail["items"][0]["current_report_head"], true, "{detail}");
                &detail["items"][0]
            } else {
                &detail["data"]["revision"]
            };
            assert_eq!(evidence["report_id"], json!(report.report_id), "{detail}");
        }
        let (status, page) = supervised(&machine, token, "result.list", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"][0]["report_id"], json!(report.report_id));
        let (status, view) = supervised(&machine, token, "attempt.show", attempt.clone()).await;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(view["attempt_id"], json!(report.binding.attempt_id));
        assert_eq!(
            view.get("task_assessment").is_some(),
            has_task_assessment,
            "{view}"
        );
    }
    for (name, params) in results {
        refused(supervised(&machine, &foreign, name, params).await);
    }
    refused(supervised(&machine, &foreign, "attempt.show", attempt).await);
    refused(supervised(&machine, &foreign, "result.list", json!({"project_id":own})).await);
    let (status, page) = supervised(&machine, &foreign, "result.list", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page["items"].as_array().unwrap().is_empty(), "{page}");
}
