//! The sources API (spec 0013 §4.4-§4.5), generic over the host server so
//! the server adapter only says who the owner is and how an [`Error`] looks.
//! Every route is owner-only; a supervisor reaches them as its approving
//! owner, through the server's admin nest.
use crate::{Error, Server, SourceKind, SourceRecord, complete, store, view};
use aq_internal_work::Purpose;
use aq_playbooks_compiler::{docs, draft};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, State},
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeSet;
use uuid::Uuid;

/// The scope every playbook of the namespace runs in.
pub const SCOPE: &str = "project";
/// Room for the engine's largest document (4 MiB) and its JSON escaping.
const BODY_BYTES: usize = 6 * 1024 * 1024;

pub fn routes<S, O, E>() -> Router<S>
where
    S: Clone + Server,
    O: FromRequestParts<S> + Into<Uuid> + Send + 'static,
    E: From<Error> + IntoResponse + 'static,
{
    Router::new()
        .route(
            "/api/playbooks/sources",
            get(list::<S, O, E>).post(register::<S, O, E>),
        )
        .route(
            "/api/playbooks/sources/{id}",
            get(read::<S, O, E>).delete(remove::<S, O, E>),
        )
        .route(
            "/api/playbooks/sources/{id}/markdown",
            put(markdown::<S, O, E>),
        )
        .route("/api/playbooks/sources/{id}/runs", post(again::<S, O, E>))
        .route(
            "/api/playbooks/sources/{id}/rollback",
            post(rollback::<S, O, E>),
        )
        .layer(DefaultBodyLimit::max(BODY_BYTES))
}

async fn one<S: Server>(s: &S, id: &str) -> Result<Value, Error> {
    let pool: &PgPool = s.as_ref();
    let record = (store::get(pool, id).await?)
        .ok_or_else(|| Error::NotFound(format!("There is no playbook source {id}")))?;
    view::view(pool, &record, s.now_ms()).await
}

async fn list<S: Server, O, E: From<Error>>(State(s): State<S>, _: O) -> Result<Json<Value>, E> {
    Ok(Json(view::list(s.as_ref(), s.now_ms()).await?))
}

async fn read<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<String>,
) -> Result<Json<Value>, E> {
    Ok(Json(one(&s, &id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    id: String,
    binding: String,
    purpose: String,
    markdown: String,
    commands: BTreeSet<String>,
    events: BTreeSet<String>,
}

fn name(what: &str, value: &str, max: usize) -> Result<(), Error> {
    let fits = !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|c| c.is_control() || c == '/');
    match fits {
        true => Ok(()),
        false => Err(Error::Invalid(format!(
            "{what} must be 1 to {max} bytes without control characters or '/'"
        ))),
    }
}

/// Registers a new source and starts authoring it. What its playbook may do follows from the commands
/// it names: the abilities those commands need, and no more.
async fn register<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Json(body): Json<Register>,
) -> Result<Json<Value>, E> {
    name("id", &body.id, 128)?;
    name("binding", &body.binding, 128)?;
    if body.purpose.trim().is_empty() || body.purpose.len() > 1024 {
        return Err(Error::Invalid("purpose must be 1 to 1024 bytes".into()).into());
    }
    let (_, capabilities) =
        draft::retain_registry(&docs::catalog_registry(), &body.commands, &body.events)
            .map_err(|e| Error::Invalid(format!("{e}; name commands and events of the catalog")))?;
    if capabilities.is_empty() {
        return Err(Error::Invalid("a source needs at least one command".into()).into());
    }
    let record = SourceRecord {
        id: body.id,
        kind: SourceKind::Registered,
        binding: body.binding,
        scope: SCOPE.into(),
        purpose: body.purpose,
        judgment_purpose: Purpose::PlaybookCall,
        commands: body.commands,
        events: body.events,
        capabilities,
        facts: vec![],
        revision: 0,
    };
    // Create only: a source that exists keeps its document, binding and
    // abilities. Its document changes through `PUT …/markdown` at a revision.
    if store::get(s.as_ref(), &record.id).await?.is_some() {
        return Err(Error::Conflict(format!(
            "The source {} already exists; change its document with its expected revision",
            record.id
        ))
        .into());
    }
    store::register(s.as_ref(), &record, &body.markdown).await?;
    complete::author(&s, &record.id).await?;
    Ok(Json(one(&s, &record.id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Markdown {
    markdown: String,
    expected_revision: u64,
}

async fn markdown<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<String>,
    Json(body): Json<Markdown>,
) -> Result<Json<Value>, E> {
    store::set_markdown(s.as_ref(), &id, &body.markdown, body.expected_revision).await?;
    complete::author(&s, &id).await?;
    Ok(Json(one(&s, &id).await?))
}

async fn again<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<String>,
) -> Result<Json<Value>, E> {
    complete::author(&s, &id).await?;
    Ok(Json(one(&s, &id).await?))
}

async fn rollback<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<String>,
) -> Result<Json<Value>, E> {
    complete::rollback(&s, &id).await?;
    Ok(Json(one(&s, &id).await?))
}

async fn remove<S: Server, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<String>,
) -> Result<Json<Value>, E> {
    complete::remove(&s, &id).await?;
    Ok(Json(json!({"deleted": true})))
}
