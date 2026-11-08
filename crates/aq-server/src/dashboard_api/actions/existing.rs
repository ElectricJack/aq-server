//! Common wiring for the three existing human domain owners.
use super::*;
use crate::error::AppError;
use sqlx::Row;

pub(crate) fn registry(
    state: &AppState,
    reviews: Option<Arc<crate::reviews::Service>>,
    playbooks: Option<Arc<crate::playbooks::driver::Driver>>,
) -> Registry {
    let mut registry = Registry::default();
    registry
        .register(crate::agent_access::human::TaskCreate {
            runtime: Arc::new(crate::agent_access::human::ReadRuntime::new(state)),
            original: None,
        })
        .expect("static task adapter");
    registry
        .register(crate::agent_access::supervisor_conversation::SendMessage(
            Arc::new(crate::agent_access::supervisor_conversation::Service::new(
                state,
            )),
        ))
        .expect("static message adapter");
    registry
        .register(crate::reviews::action::ResolveDecision(reviews))
        .expect("static decision adapter");
    crate::results_feedback::actions::register(&mut registry);
    crate::agent_access::human::graph_actions::register(state, &mut registry);
    crate::agent_access::human::task_execution::register(state, &mut registry);
    crate::agent_access::messaging::session_action::register(&mut registry);
    crate::agent_access::questions::action::register(&mut registry);
    crate::machine_actions::register(&mut registry);
    crate::agent_access::human::comments::register(&mut registry);
    // Unknown kinds are refused when no playbook runtime is mounted.
    if let Some(driver) = playbooks {
        crate::playbooks::actions::register(driver, &mut registry);
    }
    registry
}
pub(crate) fn schemas() -> Registry {
    // Schema generation is independent of configured domain services.
    let mut registry = Registry::default();
    registry
        .register_schema::<crate::agent_access::human::TaskCreate>()
        .expect("task schema");
    registry
        .register_schema::<crate::agent_access::supervisor_conversation::SendMessage>()
        .expect("message schema");
    registry
        .register_schema::<crate::reviews::action::ResolveDecision>()
        .expect("decision schema");
    crate::results_feedback::actions::schemas(&mut registry);
    crate::agent_access::human::graph_actions::schemas(&mut registry);
    crate::agent_access::human::task_execution::schemas(&mut registry);
    crate::agent_access::messaging::session_action::schemas(&mut registry);
    crate::agent_access::questions::action::schemas(&mut registry);
    crate::machine_actions::schemas(&mut registry);
    crate::agent_access::human::comments::schemas(&mut registry);
    crate::playbooks::actions::schemas(&mut registry);
    registry
}
pub(crate) async fn owner(state: &AppState, user: &User) -> Result<(), ApiError> {
    let row = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1")
        .bind(user.id)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or_else(|| ApiError::for_status(StatusCode::FORBIDDEN))?;
    if row.get::<String, _>("role") != "owner"
        || row.get::<String, _>("email") != user.email
        || row.get::<Option<String>, _>("issuer") != user.issuer
        || row.get::<Option<String>, _>("subject") != user.subject
    {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(())
}
pub(crate) fn request(
    kind: &str,
    id: Uuid,
    entity: &str,
    subject: Uuid,
    revision: String,
    payload: serde_json::Value,
    version_key: &str,
) -> ActionRequest {
    ActionRequest {
        operation_id: id,
        kind: kind.into(),
        subject: EntityRef {
            entity_type: entity.into(),
            id: subject.to_string(),
            revision: Some(revision.clone()),
        },
        expected_versions: std::collections::BTreeMap::from([(version_key.into(), revision)]),
        reason: None,
        payload,
    }
}
pub(crate) fn subject(
    request: &ActionRequest,
    entity: &str,
    key: &str,
    expected: &str,
) -> Result<Uuid, ApiError> {
    let id = request
        .subject
        .id
        .parse::<Uuid>()
        .map_err(|_| ApiError::invalid())?;
    if id.is_nil()
        || request.subject.entity_type != entity
        || request.subject.revision.as_deref() != Some(expected)
        || request.expected_versions
            != std::collections::BTreeMap::from([(key.into(), expected.into())])
    {
        return Err(ApiError::invalid());
    }
    Ok(id)
}
pub(crate) fn disposition(disposition: Disposition) -> Outcome {
    Outcome {
        disposition,
        receipt_refs: vec![],
        affected_entities: vec![],
        current_versions: Default::default(),
    }
}
pub(crate) fn failure(error: AppError) -> Outcome {
    disposition(match error.status().as_u16() {
        409 => Disposition::Conflict,
        400 | 401 | 403 | 404 | 413 | 422 => Disposition::Rejected,
        _ => Disposition::Unknown,
    })
}
pub(crate) fn entity(kind: &str, id: impl ToString, revision: Option<String>) -> EntityRef {
    EntityRef {
        entity_type: kind.into(),
        id: id.to_string(),
        revision,
    }
}
pub(crate) fn legacy_error(error: ApiError) -> AppError {
    let text = "dashboard operation unavailable or conflicting".into();
    match error.status().as_u16() {
        401 => AppError::Unauthenticated(text),
        403 => AppError::Forbidden(text),
        404 => AppError::NotFound(text),
        409 => AppError::Conflict(text),
        413 => AppError::PayloadTooLarge(text),
        422 => AppError::BadRequest(text),
        _ => AppError::IdentityUnavailable(text),
    }
}
