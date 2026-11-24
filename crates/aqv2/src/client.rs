use crate::{Result, actions, cli::*, error, unsupported, with_operation};
use aq_human_http::Client;
use serde_json::Value;
use uuid::Uuid;

pub fn http_error(err: aq_human_http::Error) -> Value {
    use aq_human_http::Error;
    let code = match err {
        Error::Status(404) => "not_found",
        Error::Status(426 | 501) => "unsupported",
        Error::Status(422) => "invalid_request",
        Error::Status(_) => "http_error",
        Error::Transport => "transport_error",
        Error::Configuration => "configuration_error",
        Error::Credential => "credential_error",
        Error::RequestBound => "request_too_large",
        Error::ResponseBound | Error::Response => "invalid_response",
    };
    let mut value = error(code, &err.to_string());
    if let Error::Status(status) = err {
        value["http_status"] = status.into();
    }
    value
}
pub(crate) fn response(value: aq_human_http::JsonResponse) -> Result<Value> {
    if (200..300).contains(&value.status) {
        return Ok(value.body);
    }
    let mut err = http_error(aq_human_http::Error::Status(value.status));
    // Consume only known protocol codes and UUIDs. Never reflect raw error bodies
    // or server messages (including a proxy's HTML error response).
    if let Some(code) = value.body["code"].as_str().filter(|code| {
        matches!(
            *code,
            "unknown_action_kind"
                | "unsupported_contract"
                | "unauthenticated"
                | "forbidden"
                | "not_found"
                | "operation_conflict"
                | "expired"
                | "request_too_large"
                | "capacity_exceeded"
                | "unavailable"
                | "invalid_request"
                | "cursor_restart_required"
                | "replacement_project_revision_changed"
                | "fresh_retry_not_applicable"
        )
    }) {
        err["server_code"] = code.into();
        if matches!(code, "unknown_action_kind" | "unsupported_contract") {
            err["code"] = "unsupported".into();
        }
    }
    if value.status == 422 && err["server_code"] == "replacement_project_revision_changed" {
        err["message"] = "The task's project binding changed. To start from the current project source, use aq task retry with --fresh true.".into();
    }
    if let Some(id) = value.body["request_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    {
        err["request_id"] = id.to_string().into();
    }
    Err(err)
}
pub async fn read(http: &Client, path: &str, query: &[(String, String)]) -> Result<Value> {
    let query: Vec<_> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let value = http
        .get_json_response(path, &query)
        .await
        .map_err(http_error)?;
    // Resource-qualified routes may return a neutral 404 for missing or hidden
    // resources. Only unambiguous collection/discovery routes imply absence.
    if matches!(value.status, 404 | 405 | 501)
        && matches!(
            path,
            "/api/v1/projects"
                | "/api/human/overview/tasks"
                | "/api/reviews/decisions"
                | "/api/human/conversations/global"
        )
    {
        return Err(unsupported("requested read"));
    }
    response(value)
}
fn page(page: &Page) -> Vec<(String, String)> {
    let mut query = vec![];
    if let Some(limit) = page.limit {
        query.push(("limit".into(), limit.to_string()));
    }
    if let Some(cursor) = &page.cursor {
        query.push(("cursor".into(), cursor.clone()));
    }
    query
}
pub(crate) fn validate_receipt(value: &Value, id: Uuid) -> Result<()> {
    let invalid = || {
        with_operation(
            error(
                "invalid_response",
                "Operation response does not match the original operation ID or contract",
            ),
            id,
        )
    };
    let envelope: crate::wire::Envelope =
        serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    let matching_operation =
        envelope.api_version == "dashboard-actions-1" && envelope.data.operation_id == id;
    let verified_request =
        !envelope.request_id.is_nil() && valid_digest(&envelope.data.request_digest);
    if !matching_operation || !verified_request {
        return Err(invalid());
    }
    Ok(())
}
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub async fn submit(http: &Client, request: &actions::ActionRequest) -> Result<Value> {
    let id = request.operation_id;
    let raw = http
        .post_json_response("/api/v1/actions", request)
        .await
        .map_err(|e| with_operation(http_error(e), id))?;
    let value = response(raw).map_err(|e| with_operation(e, id))?;
    validate_receipt(&value, id)?;
    if value["data"]["disposition"] != "confirmed" {
        let mut err = with_operation(
            error(
                "operation_unconfirmed",
                "Operation is not confirmed; inspect its original ID before taking further action",
            ),
            id,
        );
        err["operation"] = value;
        return Err(err);
    }
    Ok(value)
}
/// The change expects `--revision`, or the version the task has now. The
/// server compares it again, so a task that moves in between is a conflict.
async fn task_change(http: &Client, kind: &'static str, c: &TaskChange) -> Result<Value> {
    let revision = match &c.revision {
        Some(r) => r.clone(),
        None => {
            let path = format!("/api/human/projects/{}/tasks/{}", c.project, c.id);
            let task = read(http, &path, &[]).await?;
            task["data"]["version"]
                .as_str()
                .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .map(str::to_string)
                .ok_or_else(|| error("invalid_response", "The task read carries no version"))?
        }
    };
    submit(http, &actions::task_change(kind, c, revision)).await
}
pub async fn execute(http: &Client, command: &Command) -> Result<Value> {
    if let Some(operation) = command.unsupported() {
        return Err(unsupported(operation));
    }
    match command {
        Command::Tasks(command) => tasks(http, command).await,
        Command::Workers(Workers::Revoke { machine }) => {
            submit(http, &actions::worker_revoke(machine)).await
        }
        Command::Decisions(command) => decisions(http, command).await,
        Command::Messages(Messages::Inbox(history)) => crate::chat::history(http, history).await,
        Command::Messages(Messages::Send { text }) => crate::chat::send(http, text).await,
        Command::Chat(args) => crate::chat::chat(http, args).await,
        Command::Workers(Workers::List) => read(http, "/api/workers", &[]).await,
        Command::Projects(Projects::List { page: p }) => {
            read(http, "/api/v1/projects", &page(p)).await
        }
        Command::Projects(Projects::Show { id }) => {
            read(http, &format!("/api/projects/{id}"), &[]).await
        }
        Command::Operations(Operations::Show { id }) => {
            let value = read(http, &format!("/api/v1/operations/{id}"), &[]).await?;
            validate_receipt(&value, *id)?;
            Ok(value)
        }
        _ => Err(unsupported("requested action")),
    }
}
async fn tasks(http: &Client, command: &Tasks) -> Result<Value> {
    match command {
        Tasks::List { project, page: p } => {
            let path = project
                .map(|id| format!("/api/human/projects/{id}/tasks"))
                .unwrap_or_else(|| "/api/human/overview/tasks".into());
            let mut query = page(p);
            if project.is_none() {
                for (key, _) in &mut query {
                    if key == "limit" {
                        *key = "task_limit".into();
                    } else if key == "cursor" {
                        *key = "after".into();
                    }
                }
            }
            read(http, &path, &query).await
        }
        Tasks::Show { id, project } => {
            read(
                http,
                &format!("/api/human/projects/{project}/tasks/{id}"),
                &[],
            )
            .await
        }
        Tasks::Create(args) => submit(http, &actions::task(args)).await,
        Tasks::Cancel(c) => task_change(http, "task.cancel", c).await,
        Tasks::Retry(c) => task_change(http, "task.retry", c).await,
    }
}
async fn decisions(http: &Client, command: &Decisions) -> Result<Value> {
    match command {
        Decisions::List {
            project,
            state,
            page: p,
        } => {
            let mut query = page(p);
            if let Some(id) = project {
                query.push(("project_id".into(), id.to_string()));
            }
            if matches!(state, DecisionState::Closed) {
                query.push(("state".into(), "closed".into()));
            }
            read(http, "/api/reviews/decisions", &query).await
        }
        Decisions::Show { id } => read(http, &format!("/api/reviews/decisions/{id}"), &[]).await,
        Decisions::Respond { id, request } => submit(http, &actions::decision(*id, request)?).await,
    }
}
