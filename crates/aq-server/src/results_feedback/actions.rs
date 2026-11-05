use super::{contract::*, store};
use crate::{
    AppState,
    dashboard_api::{
        actions::{ActionKind, ActionRequest, Disposition, Operation, Outcome, Registry, existing},
        contract::Scope,
        errors::ApiError,
    },
    store::User,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ApiError>;
pub(crate) trait Payload:
    Clone + serde::de::DeserializeOwned + schemars::JsonSchema + Send + Sync + 'static
{
    const KIND: &'static str;
    const ENTITY: &'static str;
    const VERSION_KEY: &'static str;
    fn command(self) -> Command;
}
pub(crate) struct Adapter<P>(std::marker::PhantomData<P>);
impl<P> Default for Adapter<P> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}
macro_rules! payload {
    ($p:ty,$kind:literal,$entity:literal,$key:literal,$variant:ident) => {
        impl Payload for $p {
            const KIND: &'static str = $kind;
            const ENTITY: &'static str = $entity;
            const VERSION_KEY: &'static str = $key;
            fn command(self) -> Command {
                Command::$variant(self)
            }
        }
    };
}
payload!(Follow, "result.follow", "result", "result", Follow);
payload!(Comment, "result.comment", "result", "result", Comment);
payload!(
    Correction,
    "comment.revise",
    "comment",
    "comment",
    Correction
);
payload!(Resolve, "thread.resolve", "thread", "thread", Resolve);
payload!(
    FeedbackDraft,
    "feedback.task_create",
    "result",
    "thread",
    Draft
);
payload!(FeedbackLink, "feedback.link", "result", "thread", Link);
payload!(
    Waiver,
    "result.evidence_waive",
    "result",
    "assessment",
    Waiver
);

pub(crate) fn register(r: &mut Registry) {
    r.register(Adapter::<Follow>::default())
        .expect("follow adapter");
    r.register(Adapter::<Comment>::default())
        .expect("comment adapter");
    r.register(Adapter::<Correction>::default())
        .expect("correction adapter");
    r.register(Adapter::<Resolve>::default())
        .expect("resolve adapter");
    r.register(Adapter::<FeedbackDraft>::default())
        .expect("feedback adapter");
    r.register(Adapter::<FeedbackLink>::default())
        .expect("link adapter");
    r.register(Adapter::<Waiver>::default())
        .expect("waiver adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<Adapter<Follow>>()
        .expect("follow schema");
    r.register_schema::<Adapter<Comment>>()
        .expect("comment schema");
    r.register_schema::<Adapter<Correction>>()
        .expect("correction schema");
    r.register_schema::<Adapter<Resolve>>()
        .expect("resolve schema");
    r.register_schema::<Adapter<FeedbackDraft>>()
        .expect("draft schema");
    r.register_schema::<Adapter<FeedbackLink>>()
        .expect("link schema");
    r.register_schema::<Adapter<Waiver>>()
        .expect("waiver schema");
}
pub(super) fn request<P: Payload + serde::Serialize>(id: Uuid, p: P) -> ActionRequest {
    let cmd = p.clone().command();
    existing::request(
        P::KIND,
        cmd.id(),
        P::ENTITY,
        id,
        cmd.version().into(),
        json!(p),
        P::VERSION_KEY,
    )
}
fn native(state: &AppState) -> crate::agent_access::human::TaskCreate {
    crate::agent_access::human::TaskCreate {
        runtime: Arc::new(crate::agent_access::human::ReadRuntime::new(state)),
        original: None,
    }
}
fn native_request(v: &FeedbackDraft) -> ActionRequest {
    existing::request(
        "task.create",
        v.operation_id,
        "project",
        v.project_id,
        v.expected_project_revision.clone(),
        json!({"operation_id":v.operation_id,"expected_project_revision":v.expected_project_revision,"title":v.title,"body":v.description,"acceptance_criteria":v.acceptance_criteria,"labels":["feedback"],"task_state":"draft","parent_id":v.parent_id}),
        "project",
    )
}
#[async_trait]
impl<P: Payload> ActionKind for Adapter<P> {
    type Payload = P;
    const KIND: &'static str = P::KIND;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &P,
    ) -> Result<Scope> {
        let cmd = payload.clone().command();
        if cmd.id() != request.operation_id {
            return Err(ApiError::invalid());
        }
        let id = existing::subject(request, P::ENTITY, P::VERSION_KEY, cmd.version())?;
        if let Some(p) = store::effect_pin(state, user, cmd.id()).await? {
            // Replays validate current authority, never current mutable preconditions.
            store::detail(state, user, &p).await?;
            return Ok(Scope::Project {
                project_id: p.project,
            });
        }
        let p = if let Command::Draft(v) = &cmd {
            if let Some(retained) = store::retained_draft(state, user, v.operation_id).await? {
                let p = store::pin(state, retained.result_id, Some(&retained.revision)).await?;
                store::detail(state, user, &p).await?;
                p
            } else {
                store::subject(state, user, id, &cmd).await?
            }
        } else {
            store::subject(state, user, id, &cmd).await?
        };
        match cmd {
            Command::Draft(v) => {
                if let Some(parent) = v.parent_id {
                    crate::agent_access::human::authorize_task(state, user, v.project_id, parent)
                        .await?;
                }
                let r = native_request(&v);
                let adapter = native(state);
                adapter
                    .authorize(
                        state,
                        user,
                        &r,
                        &serde_json::from_value(r.payload.clone())
                            .map_err(|_| ApiError::invalid())?,
                    )
                    .await?;
            }
            Command::Link(v) => {
                crate::agent_access::human::authorize_task(state, user, v.project_id, v.task_id)
                    .await?;
            }
            _ => {}
        }
        Ok(Scope::Project {
            project_id: p.project,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        op: Option<&Operation>,
    ) -> Result<()> {
        store::principal(state, user).await?;
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        if let Some(op) = op {
            let reference = op.subject_refs.first().ok_or_else(ApiError::invalid)?;
            let id = reference
                .id
                .parse::<Uuid>()
                .map_err(|_| ApiError::invalid())?;
            let report:Option<Uuid>=match P::ENTITY {
                "comment"=>sqlx::query_scalar("SELECT t.report_id FROM result_comments c JOIN result_threads t ON t.id=c.thread_id WHERE c.id=$1").bind(id).fetch_optional(&state.store.pool).await?,
                "thread"=>sqlx::query_scalar("SELECT report_id FROM result_threads WHERE id=$1").bind(id).fetch_optional(&state.store.pool).await?,
                _=>Some(id),
            };
            let p = store::pin(state, report.ok_or_else(store::missing)?, None).await?;
            if p.project != *project_id {
                return Err(store::missing());
            }
            if op.disposition == Disposition::Confirmed {
                let effect = store::effect(state, user, op.operation_id)
                    .await?
                    .ok_or_else(store::missing)?;
                for reference in &op.affected_entities {
                    if reference.entity_type == "task" {
                        let task = reference.id.parse::<Uuid>().map_err(|_| store::missing())?;
                        crate::agent_access::human::authorize_task(state, user, p.project, task)
                            .await?;
                    }
                }
                if effect.receipt_refs != op.receipt_refs
                    || effect.affected_entities != op.affected_entities
                {
                    return Err(store::missing());
                }
            }
        }
        Ok(())
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: P,
    ) -> Outcome {
        let cmd = payload.command();
        let id = match request.subject.id.parse::<Uuid>() {
            Ok(v) => v,
            Err(_) => return Outcome::unknown(),
        };
        let result = async {
            if let Some(old) = store::effect(state, user, cmd.id()).await? {
                return Ok(old);
            }
            if let Command::Draft(v) = &cmd {
                let p = store::subject(state, user, id, &cmd).await?;
                store::draft_intent(state, user, &p, v).await?;
                let r = native_request(v);
                let adapter = native(state);
                let native = adapter
                    .execute(
                        state,
                        user,
                        &r,
                        serde_json::from_value(r.payload.clone())
                            .map_err(|_| ApiError::invalid())?,
                    )
                    .await;
                return store::finish_draft(state, user, &p, v, native).await;
            }
            store::apply(state, user, id, &cmd).await
        }
        .await;
        match result {
            Ok(outcome) => outcome,
            Err(e) => existing::disposition(match e.status().as_u16() {
                409 => Disposition::Conflict,
                400 | 401 | 403 | 404 | 413 | 422 => Disposition::Rejected,
                _ => Disposition::Unknown,
            }),
        }
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        op: &Operation,
    ) -> Result<Option<Outcome>> {
        if let Some(old) = store::effect(state, user, op.operation_id).await? {
            return Ok(Some(old));
        }
        if P::KIND != FeedbackDraft::KIND {
            return Ok(None);
        }
        let Some(v) = store::retained_draft(state, user, op.operation_id).await? else {
            return Ok(None);
        };
        let p = store::pin(state, v.result_id, Some(&v.revision)).await?;
        let adapter = native(state);
        let outcome = adapter.reconcile(state, user, op).await?;
        if let Some(outcome) = outcome {
            return store::finish_draft(state, user, &p, &v, outcome)
                .await
                .map(Some);
        }
        Ok(None)
    }
}

/// Explicit recovery may dispatch; ordinary lookup only reconciles local links.
pub(super) async fn recover(state: &AppState, user: &User, id: Uuid) -> Result<Operation> {
    let v = store::retained_draft(state, user, id)
        .await?
        .ok_or_else(store::missing)?;
    let request = request(v.result_id, v.clone());
    Adapter::<FeedbackDraft>::default()
        .authorize(state, user, &request, &v)
        .await?;
    if store::effect(state, user, id).await?.is_none() {
        let p = store::pin(state, v.result_id, Some(&v.revision)).await?;
        let adapter = native(state);
        let r = native_request(&v);
        let outcome = match adapter.recover(state, user, id).await? {
            Some(outcome) => outcome,
            None => {
                adapter
                    .execute(
                        state,
                        user,
                        &r,
                        serde_json::from_value(r.payload.clone())
                            .map_err(|_| ApiError::invalid())?,
                    )
                    .await
            }
        };
        store::finish_draft(state, user, &p, &v, outcome).await?;
    }
    let registry = existing::registry(state, None, None);
    crate::dashboard_api::actions::submit_legacy(state, &registry, user, request).await
}

pub(super) async fn draft_status(
    state: &AppState,
    user: &User,
    id: Uuid,
) -> Result<(Uuid, serde_json::Value)> {
    let v = store::retained_draft(state, user, id)
        .await?
        .ok_or_else(store::missing)?;
    let outcome = native(state).retained_outcome(state, user, id).await?;
    let disposition = outcome
        .as_ref()
        .map(|o| o.disposition)
        .unwrap_or(Disposition::Pending);
    let link = store::effect(state, user, id).await?.is_some();
    Ok((
        v.project_id,
        json!({"operation_id":id,"task_id":id,"creation_disposition":disposition,"link_confirmed":link,"result_id":v.result_id,"revision":v.revision,"origin":v}),
    ))
}
