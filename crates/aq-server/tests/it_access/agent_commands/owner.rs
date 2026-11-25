use super::*;
use crate::messaging::Env;
mod cases;
pub(super) async fn whoami_and_waits(e: &Env) {
    let (status, me) = send(&e.admin, "/api/v1/commands/whoami", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"], "owner");
    // A read: the command's response is the REST route's response.
    let (status, direct) = send(&e.admin, "/api/waits", None, None).await;
    assert_eq!(status, StatusCode::OK, "{direct}");
    let (status, via) = send(
        &e.admin,
        "/api/v1/commands/wait.list",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{via}");
    assert_eq!(via, direct);
    // An undeclared query param is refused before the route is asked.
    let (status, refused) = send(
        &e.admin,
        "/api/v1/commands/wait.list",
        None,
        Some(json!({"params":{"projet_id":e.binding["project_id"]}})),
    )
    .await;
    assert_eq!(
        (status, refused["code"].clone(), refused["message"].clone()),
        (
            StatusCode::BAD_REQUEST,
            json!("invalid_request"),
            json!("unknown param")
        ),
        "{refused}"
    );
}
pub(super) async fn read_routes(e: &Env) {
    // The owner reads registered from REST answer as their routes do,
    // whatever the route answers in this fixture.
    let project = e.binding["project_id"].as_str().unwrap();
    for (name, path, params, same_body) in cases::reads(project) {
        let (direct_status, direct) = send(&e.admin, &path, None, None).await;
        let (status, via) = send(
            &e.admin,
            &format!("/api/v1/commands/{name}"),
            None,
            Some(json!({"params":params})),
        )
        .await;
        assert_eq!(status, direct_status, "{name}: {via} / {direct}");
        // Bodies with clocks or request IDs are compared by status only.
        if same_body && status == StatusCode::OK {
            assert_eq!(via, direct, "{name}");
        }
    }
}
pub(super) async fn discovery(e: &Env) {
    let (status, listed) = send(&e.admin, "/api/v1/commands", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listed["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"decision.resolve") && names.contains(&"task.show"));
    assert!(
        !names.contains(&"wait.register"),
        "agent-only commands are not the owner's"
    );
}
pub(super) async fn missing_subject(e: &Env) {
    // A mutation: the command builds the same ActionRequest as /api/v1/actions.
    let (status, body) = send(
        &e.admin,
        "/api/v1/commands/task.comment.add",
        None,
        Some(json!({"params":{"operation_id":Uuid::new_v4(),"project_id":e.binding["project_id"],"body":"x"}})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_request")),
        "subject is required: {body}"
    );
}
pub(super) async fn mutation_routes(e: &Env) {
    // A mutation served by its own REST route answers as that route does for
    // the same body (each with its own operation ID, so neither is a replay).
    let project = e.binding["project_id"].as_str().unwrap();
    for (name, path, params) in cases::mutations(project) {
        // The direct body is the params without the path params.
        let mut body = params.clone();
        for key in [
            "project_id",
            "attempt_id",
            "conversation_id",
            "session_id",
            "wait_id",
            "continuation_id",
            "operation",
        ] {
            body.as_object_mut().unwrap().remove(key);
        }
        let takes_operation = !matches!(
            name,
            "wait.suspend" | "sandbox.release" | "operation.recover"
        );
        if takes_operation {
            body["operation_id"] = json!(Uuid::new_v4());
        }
        let (direct_status, direct) = send(&e.admin, &path, None, Some(body)).await;
        let (status, via) = send(
            &e.admin,
            &format!("/api/v1/commands/{name}"),
            None,
            Some(json!({"operation_id":Uuid::new_v4(),"params":params})),
        )
        .await;
        assert_eq!(status, direct_status, "{name}: {via} / {direct}");
        assert!(
            !matches!(
                status,
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::UNSUPPORTED_MEDIA_TYPE
            ) && via["message"] != "unknown param",
            "{name}: {status} {via}"
        );
    }
}
pub(super) async fn registered_routes(e: &Env) {
    // Playbook sources, internal work and routing: each command answers as
    // its route does for the same method and body, whatever that is here.
    for cases::Route {
        name,
        method,
        path,
        params,
        path_param,
    } in cases::registry()
    {
        let mut body = params.clone();
        if let Some(key) = path_param {
            body.as_object_mut().unwrap().remove(key);
        }
        let direct = send_as(&e.admin, method, &path, (method != "GET").then_some(body)).await;
        let via = send_as(
            &e.admin,
            "POST",
            &format!("/api/v1/commands/{name}"),
            Some(json!({"params":params})),
        )
        .await;
        assert_eq!(via.0, direct.0, "{name}: {} / {}", via.1, direct.1);
        assert!(
            !matches!(
                via.0,
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::UNSUPPORTED_MEDIA_TYPE
            ) && !via.1.contains("unknown param"),
            "{name}: {} {}",
            via.0,
            via.1
        );
    }
    // A filter the run list does not declare is refused before the route.
    let (status, refused) = send(
        &e.admin,
        "/api/v1/commands/internal.run.list",
        None,
        Some(json!({"params":{"state":"failed"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
}
pub(super) async fn action_subject(e: &Env) {
    // With the task named, the command builds the subject itself and answers
    // exactly as the action union does for the same request.
    let (op, task) = (Uuid::new_v4(), Uuid::new_v4());
    let payload = json!({"operation_id":op,"project_id":e.binding["project_id"],"body":"x"});
    let (direct_status, direct) = send(
        &e.admin,
        "/api/v1/actions",
        None,
        Some(
            json!({"operation_id":op,"kind":"task.comment.add","subject":{"type":"task","id":task},
            "expected_versions":{},"payload":payload}),
        ),
    )
    .await;
    let mut params = payload.clone();
    params["task_id"] = json!(task);
    let (status, via) = send(
        &e.admin,
        "/api/v1/commands/task.comment.add",
        None,
        Some(json!({"operation_id":op,"params":params})),
    )
    .await;
    assert_ne!(via["message"], "an action needs a subject", "{via}");
    assert_eq!(
        (status, via["code"].clone()),
        (direct_status, direct["code"].clone()),
        "{via} / {direct}"
    );
    let owner_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM command_audit WHERE caller_kind='owner'")
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert!(owner_rows >= 3);
}
