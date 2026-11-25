//! Requests only describe the caller's own task. Approved resources are
//! provisioned by the node once the target policy and fresh attempt agree.
use super::*;
use crate::project_bindings::workspace::{Caller, RequestBody, apply};
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/agent/workspace/request", post(request))
        .layer(DefaultBodyLimit::max(4096))
}
async fn request(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RequestBody>,
) -> Response {
    let id = Uuid::new_v4();
    let caller = match grant_check::agent_caller(&s, &headers).await {
        Ok(a) => a,
        Err(code) => {
            return grant_check::refused(
                &headers,
                aq_agent_grants::Refusal::error(
                    code,
                    "task.workspace.request",
                    "No active attempt credential",
                ),
                id,
            );
        }
    };
    let spec = crate::commands::registry()
        .get("task.workspace.request")
        .expect("registered");
    let (decision, target) = caller
        .authorize(&s, spec, &mut serde_json::Map::new())
        .await;
    let checked = aq_agent_grants::GrantRequest::new(
        spec.name,
        aq_agent_grants::op::WORKSPACE_REQUEST,
        target,
    );
    if grant_check::DISPATCHED.try_with(|_| ()).is_err() {
        grant_check::audit(
            &s,
            &aq_agent_grants::AuditEntry::agent(caller.ids, checked, &decision, id),
        )
        .await;
    }
    if let aq_agent_grants::Decision::Deny(r) = decision {
        return grant_check::refused(&headers, r, id);
    }
    let home = match crate::project_bindings::current(&s, caller.ids.project_id).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    match apply(
        &s,
        &home,
        caller.ids.task_id,
        body,
        Caller::Attempt(&caller, &headers),
    )
    .await
    {
        Ok(v) => ok(v),
        Err(e) => e.into_response(),
    }
}
