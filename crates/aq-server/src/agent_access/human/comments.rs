//! Task comments wiring (spec 0004 §4.1). The model is `aq-task-comments`;
//! this module only authenticates and authorizes: the owner read, the three
//! owner dashboard actions and the per-attempt agent routes.
use super::*;
use crate::agent_access::{Error as AgentError, authenticate, id as field, ok};
use crate::dashboard_api::{
    actions::{ActionKind, ActionRequest, Disposition, Operation, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use aq_task_comments::{self as comments, Author, Command, ENTITY, Error, Thread};
use axum::{body::Bytes, http::HeaderMap, http::StatusCode, routing::post};

/// Comment page cursors are keyset positions, so they may outlive a 30 s list cursor.
const CURSOR_TTL_MS: u64 = 600_000;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    cursor: Option<String>,
    limit: Option<u16>,
}
/// `GET .../tasks/{node}/comments?cursor&limit`: newest first, 50 by default.
pub(super) async fn read(
    (State(s), Extension(r), RequireOwner(u)): (
        State<AppState>,
        Extension<Arc<ReadRuntime>>,
        RequireOwner,
    ),
    Path((p, id)): Path<(Uuid, Uuid)>,
    q: std::result::Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    let limit = comments::limit(q.limit).map_err(|_| invalid())?;
    if id.is_nil() {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let (actor, revision) = (a.actor.id, a.record.revision);
    let kind = format!("comments:{id}");
    let b = cursor::Binding {
        actor,
        project: p,
        revision,
        workspace: w,
        kind,
        limit,
        q: None,
    };
    let before = match q.cursor {
        Some(t) => Some(
            r.cursors
                .decode(&t, &b, stamp(&s))?
                .parse()
                .map_err(|_| conflict())?,
        ),
        None => None,
    };
    let thread = Thread {
        project_id: p,
        task_id: id,
    };
    let (items, next) = comments::list(&r.pool, thread, before, limit)
        .await
        .map_err(|_| unavailable())?;
    let next = match next {
        Some(seq) => Some(r.cursors.encode_for(
            &b,
            &seq.to_string(),
            cursor::validity(stamp(&s), CURSOR_TTL_MS)?,
        )?),
        None => None,
    };
    let items: Vec<Value> = items.iter().map(|c| c.view(Some(actor))).collect();
    finish(
        &s,
        &r,
        a,
        json!({"task_id":id,"items":items,"next_cursor":next}),
    )
    .await
}

pub(crate) struct Comments<C>(std::marker::PhantomData<C>);
pub(crate) fn register(r: &mut Registry) {
    r.register(Comments::<comments::AddComment>(Default::default()))
        .and_then(|_| r.register(Comments::<comments::EditComment>(Default::default())))
        .and_then(|_| r.register(Comments::<comments::DeleteComment>(Default::default())))
        .expect("task comment adapters");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<Comments<comments::AddComment>>()
        .and_then(|_| r.register_schema::<Comments<comments::EditComment>>())
        .and_then(|_| r.register_schema::<Comments<comments::DeleteComment>>())
        .expect("task comment schemas");
}
/// Add targets `{type:"task",id}` with no versions; edit and delete target
/// `{type:"task_comment",id,revision}` with `expected_versions.task_comment`.
fn target<C: Command>(request: &ActionRequest, c: &C) -> std::result::Result<Uuid, ApiError> {
    let body = c.body().map_or(Ok(()), comments::validate_body);
    let matches = request.operation_id == c.operation_id() && !c.project_id().is_nil();
    if !matches || body.is_err() {
        return Err(ApiError::invalid());
    }
    let Some(expected) = c.expected_revision() else {
        return added_task(request);
    };
    comments::revision(expected).ok_or_else(ApiError::invalid)?;
    existing::subject(request, ENTITY, ENTITY, expected)
}
fn added_task(request: &ActionRequest) -> std::result::Result<Uuid, ApiError> {
    let subject = &request.subject;
    let unversioned = subject.revision.is_none() && request.expected_versions.is_empty();
    if subject.entity_type != "task" || !unversioned {
        return Err(ApiError::invalid());
    }
    subject
        .id
        .parse::<Uuid>()
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(ApiError::invalid)
}
fn store(_: Error) -> ApiError {
    ApiError::unavailable()
}
fn confirmed(comment: Uuid, task: Uuid, revision: i64) -> Outcome {
    let c = existing::entity(ENTITY, comment, Some(revision.to_string()));
    Outcome::confirmed(
        vec![c.clone()],
        vec![c, existing::entity("task", task, None)],
    )
}
#[async_trait::async_trait]
impl<C: Command> ActionKind for Comments<C> {
    type Payload = C;
    const KIND: &'static str = C::KIND;
    const GRANT: &'static str = aq_agent_grants::op::COMMENT_WRITE;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        c: &C,
    ) -> std::result::Result<Scope, ApiError> {
        let target = target(request, c)?;
        existing::owner(state, user).await?;
        let project = c.project_id();
        if C::KIND == comments::ADD {
            authorize_task(state, user, project, target).await?;
        } else {
            let pool = &state.store.pool;
            let comment = comments::get(pool, target).await.map_err(store)?;
            let comment = comment.filter(|v| v.project_id == project);
            let comment = comment.ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
            if comment.author != (Author::Owner { user_id: user.id }) {
                let text = "Only the owner who wrote a comment can edit or delete it";
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "comment_not_author",
                    text,
                ));
            }
            // A replay reports its recorded effect even after a later delete.
            let replay = comments::operation(pool, c.operation_id())
                .await
                .map_err(store)?;
            if comment.deleted_at.is_some() && replay.is_none() {
                let text = "This comment was deleted";
                return Err(ApiError::new(StatusCode::CONFLICT, "comment_deleted", text));
            }
            authorize_project(state, user, project).await?;
        }
        Ok(Scope::Project {
            project_id: project,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        op: Option<&Operation>,
    ) -> std::result::Result<(), ApiError> {
        existing::owner(state, user).await?;
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        let Some(subject) = op.and_then(|op| op.subject_refs.first()) else {
            return Ok(());
        };
        if subject.entity_type == ENTITY {
            let id = subject.id.parse().map_err(|_| ApiError::invalid())?;
            let comment = comments::get(&state.store.pool, id).await.map_err(store)?;
            if comment.is_none_or(|c| c.project_id != *project_id) {
                return Err(ApiError::for_status(StatusCode::NOT_FOUND));
            }
        }
        Ok(())
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        op: &Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let recorded = comments::operation(&state.store.pool, op.operation_id).await;
        let owner = Author::Owner { user_id: user.id };
        Ok(recorded
            .map_err(store)?
            .filter(|r| r.kind == C::KIND && r.actor == owner && r.revision > 0)
            .map(|r| confirmed(r.comment_id, r.task_id, r.revision)))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        c: C,
    ) -> Outcome {
        let Ok(target) = target(request, &c) else {
            return Outcome::unknown();
        };
        let actor = Author::Owner { user_id: user.id };
        match comments::apply(&state.store.pool, &c, target, &actor, (state.clock)()).await {
            Ok(a) => confirmed(a.comment.comment_id, a.comment.task_id, a.revision),
            Err(Error::Conflict(now)) => {
                Outcome::conflict([(ENTITY.into(), now.to_string())].into())
            }
            Err(Error::IdConflict) => existing::disposition(Disposition::Conflict),
            Err(Error::Unavailable) => Outcome::unknown(),
            Err(_) => existing::disposition(Disposition::Rejected),
        }
    }
}

/// The task thread (the attempt's own task unless `task` names another the
/// grant reaches) and the attempt as author, from the credential.
async fn agent(
    s: &AppState,
    h: &HeaderMap,
    path: &str,
    task: Option<Uuid>,
) -> std::result::Result<(Thread, Author), Response> {
    let p = authenticate(s, h)
        .await
        .map_err(IntoResponse::into_response)?;
    crate::agent_access::grant_check::require(s, h, &p, path, task).await?;
    let b = &p.binding;
    match [
        field(b, "project_id"),
        field(b, "task_id"),
        field(b, "session_id"),
        field(b, "attempt_id"),
    ] {
        [Ok(project_id), Ok(own), Ok(session_id), Ok(attempt_id)] => Ok((
            Thread {
                project_id,
                task_id: task.unwrap_or(own),
            },
            Author::Agent {
                session_id,
                attempt_id,
            },
        )),
        _ => Err(AgentError("unauthorized").into_response()),
    }
}
fn reply(result: std::result::Result<Value, &'static str>) -> Response {
    result.map_or_else(|code| AgentError(code).into_response(), ok)
}
pub(crate) fn machine_routes() -> Router<AppState> {
    let [(add_path, _), (list_path, _), ..] = comments::AGENT_ROUTES;
    Router::new()
        .route(add_path, post(agent_add))
        .route(list_path, post(agent_list))
        .route("/v1/agent/comments/edit", post(agent_edit))
        .route("/v1/agent/comments/delete", post(agent_delete))
}
async fn agent_add(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    match agent(&s, &h, comments::AGENT_ROUTES[0].0, None).await {
        Ok((t, a)) => reply(comments::agent_add(&s.store.pool, t, &a, &body, (s.clock)()).await),
        Err(response) => response,
    }
}
async fn agent_list(State(s): State<AppState>, h: HeaderMap, body: Bytes) -> Response {
    let task = comments::agent_list_task(&body);
    match agent(&s, &h, comments::AGENT_ROUTES[1].0, task).await {
        Ok((t, _)) => reply(comments::agent_list(&s.store.pool, t, &body).await),
        Err(response) => response,
    }
}
/// Edit or delete a comment the attempt wrote.
async fn agent_edit(State(s): State<AppState>, h: HeaderMap, b: Bytes) -> Response {
    agent_change(s, h, b, 2).await
}
async fn agent_delete(State(s): State<AppState>, h: HeaderMap, b: Bytes) -> Response {
    agent_change(s, h, b, 3).await
}
async fn agent_change(s: AppState, h: HeaderMap, body: Bytes, route: usize) -> Response {
    let kind = [comments::EDIT, comments::DELETE][route - 2];
    match agent(&s, &h, comments::AGENT_ROUTES[route].0, None).await {
        Ok((t, a)) => reply(
            comments::agent_change(&s.store.pool, kind, t.project_id, &a, &body, (s.clock)()).await,
        ),
        Err(response) => response,
    }
}
