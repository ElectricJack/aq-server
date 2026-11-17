//! Route assembly and request dispatch for the thin server adapter;
//! authentication and the CSRF rule stay in the server.
//!
//! | Path under `/api/settings` | GET | PUT | DELETE |
//! |---|---|---|---|
//! | `/roles`, `/intelligence-profiles`, `/execution-profiles` | the documents of that kind | | |
//! | `/<one of those>/{id}` | one document | create or replace | delete |
//! | `/selection-defaults` | the selection defaults | replace | |
//! | `/profiles/limits` | this store's limits | replace | |
//! | `/profiles/snapshot` | the active digest and store revision | | |
//! | `/routing-document` | the routing document: `{markdown, revision}` | replace | |
//!
//! Revisions travel as decimal strings, as in the other Settings routes.
use crate::{Change, Document, Error, Mutation, Result, invalid, store};
use aq_profiles::roles::{self, Kind, Problem};
use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::MethodRouter,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const KINDS: [(&str, Kind); 3] = [
    ("roles", Kind::Role),
    ("intelligence-profiles", Kind::Intelligence),
    ("execution-profiles", Kind::Execution),
];
/// The transport bound on one request body. The size a document may have is
/// the store's `max_document_bytes` limit; this only bounds what is read
/// before that limit can be checked.
const BODY_BYTES: usize = 8 * 1024 * 1024;

pub fn routes<S: Clone + Send + Sync + 'static>(
    read: MethodRouter<S>,
    write: MethodRouter<S>,
    remove: MethodRouter<S>,
) -> Router<S> {
    let one = read.clone().merge(write.clone()).merge(remove);
    let value = read.clone().merge(write);
    // Literal paths: the server's route inventory test reads them from here.
    Router::new()
        .route("/api/settings/roles", read.clone())
        .route("/api/settings/roles/{id}", one.clone())
        .route("/api/settings/intelligence-profiles", read.clone())
        .route("/api/settings/intelligence-profiles/{id}", one.clone())
        .route("/api/settings/execution-profiles", read.clone())
        .route("/api/settings/execution-profiles/{id}", one)
        .route("/api/settings/selection-defaults", value.clone())
        .route("/api/settings/profiles/limits", value.clone())
        .route("/api/settings/routing-document", value)
        .route("/api/settings/profiles/snapshot", read)
        .layer(DefaultBodyLimit::max(BODY_BYTES))
}

enum Target {
    Documents(Kind),
    Document(Kind, String),
    SelectionDefaults,
    Limits,
    Snapshot,
    RoutingDocument,
}
fn target(path: &str) -> Result<Target> {
    let missing = || Error::NotFound("No such settings route".into());
    // The supervisor admin nest mounts these routes below its own prefix.
    const ROOT: &str = "/api/settings/";
    let at = path.find(ROOT).ok_or_else(missing)?;
    let rest = &path[at + ROOT.len()..];
    let (first, id) = match rest.split_once('/') {
        Some((first, id)) => (first, Some(id)),
        None => (rest, None),
    };
    match (first, id) {
        ("selection-defaults", None) => Ok(Target::SelectionDefaults),
        ("routing-document", None) => Ok(Target::RoutingDocument),
        ("profiles", Some("limits")) => Ok(Target::Limits),
        ("profiles", Some("snapshot")) => Ok(Target::Snapshot),
        _ => {
            let kind = KINDS
                .iter()
                .find(|(segment, _)| *segment == first)
                .map(|(_, kind)| *kind)
                .ok_or_else(missing)?;
            Ok(match id {
                Some(id) => Target::Document(kind, id.into()),
                None => Target::Documents(kind),
            })
        }
    }
}
fn value<T: serde::Serialize>(data: T) -> Result<Value> {
    serde_json::to_value(data).map_err(|_| Error::Internal)
}
/// A document as Settings shows it: the text, and the fields it parses to or
/// why it does not parse.
fn shown(document: &Document) -> Result<Value> {
    let parsed = match document.kind {
        Kind::Role => roles::parse_role(&document.markdown).map(value),
        Kind::Intelligence => roles::parse_intelligence(&document.markdown).map(value),
        Kind::Execution => roles::parse_execution(&document.markdown).map(value),
    };
    let (fields, problem) = match parsed {
        Ok(fields) => (fields?, Value::Null),
        Err(error) => (Value::Null, Value::String(error.to_string())),
    };
    Ok(json!({
        "kind": document.kind,
        "id": document.id,
        "revision": document.revision.to_string(),
        "updated_at_ms": document.updated_at_ms,
        "markdown": document.markdown,
        "fields": fields,
        "problem": problem,
    }))
}
pub async fn read(pool: &PgPool, path: &str) -> Result<Value> {
    match target(path)? {
        Target::Documents(kind) => {
            let settings = store::settings(pool).await?;
            let documents = store::list(pool, kind).await?;
            Ok(json!({
                "revision": settings.revision.to_string(),
                "active_digest": settings.active_digest,
                "documents": documents.iter().map(shown).collect::<Result<Vec<_>>>()?,
            }))
        }
        Target::Document(kind, id) => match store::get(pool, kind, &id).await? {
            Some(document) => shown(&document),
            None => Err(Error::NotFound(format!(
                "No {} `{id}` exists",
                kind.label()
            ))),
        },
        Target::SelectionDefaults => {
            let settings = store::settings(pool).await?;
            Ok(json!({
                "revision": settings.selection_defaults_revision.to_string(),
                "selection_defaults": settings.selection_defaults,
            }))
        }
        Target::Limits => {
            let settings = store::settings(pool).await?;
            Ok(json!({
                "revision": settings.limits_revision.to_string(),
                "limits": settings.limits,
            }))
        }
        Target::Snapshot => {
            let settings = store::settings(pool).await?;
            Ok(json!({
                "revision": settings.revision.to_string(),
                "active_digest": settings.active_digest,
            }))
        }
        Target::RoutingDocument => {
            let (markdown, revision) = store::routing_document(pool).await?;
            Ok(json!({"markdown": markdown, "revision": revision.to_string()}))
        }
    }
}

fn revision(text: &str) -> Result<i64> {
    text.parse::<i64>()
        .ok()
        .filter(|revision| *revision >= 0)
        .ok_or_else(|| invalid("Invalid revision"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Write {
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    selection_defaults: Option<Value>,
    #[serde(default)]
    limits: Option<Value>,
    expected_revision: String,
    operation_id: Uuid,
}
async fn apply(pool: &PgPool, actor: Uuid, mutation: Mutation, now_ms: i64) -> Result<Value> {
    let receipt = store::apply(pool, actor, &mutation, now_ms).await?;
    Ok(json!({"revision": receipt.revision.to_string(), "digest": receipt.digest}))
}
/// `PUT`: `{markdown | selection_defaults | limits, expected_revision,
/// operation_id}`, answered with `{revision, digest}`.
pub async fn write(
    pool: &PgPool,
    actor: Uuid,
    path: &str,
    body: Value,
    now_ms: i64,
) -> Result<Value> {
    let bad = || invalid("Invalid settings request");
    let body: Write = serde_json::from_value(body).map_err(|_| bad())?;
    let change = match (
        target(path)?,
        body.markdown,
        body.selection_defaults,
        body.limits,
    ) {
        (Target::Document(kind, id), Some(markdown), None, None) => {
            Change::Save { kind, id, markdown }
        }
        (Target::RoutingDocument, Some(markdown), None, None) => Change::RoutingDocument(markdown),
        (Target::SelectionDefaults, None, Some(defaults), None) => {
            Change::SelectionDefaults(serde_json::from_value(defaults).map_err(|_| bad())?)
        }
        (Target::Limits, None, None, Some(limits)) => {
            Change::Limits(serde_json::from_value(limits).map_err(|_| bad())?)
        }
        _ => return Err(bad()),
    };
    let mutation = Mutation {
        change,
        expected_revision: revision(&body.expected_revision)?,
        operation_id: body.operation_id,
    };
    apply(pool, actor, mutation, now_ms).await
}
/// `DELETE …/{id}?expected_revision=<n>&operation_id=<uuid>`. Refused with
/// the problems while another document or one of `references` names it.
pub async fn remove(
    pool: &PgPool,
    actor: Uuid,
    path: &str,
    query: Option<&str>,
    now_ms: i64,
    references: crate::References,
) -> Result<Value> {
    let bad = || invalid("Give expected_revision and operation_id in the query");
    let Target::Document(kind, id) = target(path)? else {
        return Err(Error::NotFound("No such settings route".into()));
    };
    let parameter = |name: &str| {
        query
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
            .ok_or_else(bad)
    };
    let mutation = Mutation {
        change: Change::Delete { kind, id },
        expected_revision: revision(parameter("expected_revision")?)?,
        operation_id: parameter("operation_id")?.parse().map_err(|_| bad())?,
    };
    let receipt = store::apply_referenced(pool, actor, &mutation, now_ms, Some(references)).await?;
    Ok(json!({"revision": receipt.revision.to_string(), "digest": receipt.digest}))
}

/// The response of a route: the value, or 400 with every problem of an
/// invalid set, or the server's own rendering of any other error.
pub fn respond<E: IntoResponse>(result: Result<Value>, other: impl FnOnce(Error) -> E) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(Error::Problems(problems)) => {
            let listed: Vec<String> = problems.iter().map(Problem::to_string).collect();
            let body = json!({
                "error": "bad_request",
                "message": format!("The documents are not valid: {}", listed.join("; ")),
                "problems": problems,
            });
            (StatusCode::BAD_REQUEST, Json(body)).into_response()
        }
        Err(error) => other(error).into_response(),
    }
}
