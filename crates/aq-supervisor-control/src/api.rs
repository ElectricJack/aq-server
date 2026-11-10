pub use aq_manual_sessions::Auth;
use aq_manual_sessions::Error;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone)]
pub struct Service {
    pool: PgPool,
    owner: Auth,
    choices: Arc<Vec<aq_profile_settings::ModelChoice>>,
}
impl Service {
    pub fn new(pool: PgPool, owner: Auth, choices: Vec<aq_profile_settings::ModelChoice>) -> Self {
        Self {
            pool,
            owner,
            choices: Arc::new(choices),
        }
    }
    pub fn routes<S: Clone + Send + Sync + 'static>(&self) -> Router<S> {
        Router::new()
            .route("/api/supervisors", get(list))
            .route(
                "/api/supervisors/{id}/control",
                axum::routing::post(control),
            )
            .route(
                "/api/supervisors/{id}/operations/{operation}",
                get(operation),
            )
            .layer(DefaultBodyLimit::max(16384))
            .with_state(self.clone())
    }
}
fn invalid(message: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, message)
}
fn conflict(message: &'static str) -> Error {
    Error(StatusCode::CONFLICT, message)
}
fn missing() -> Error {
    Error(StatusCode::NOT_FOUND, "Supervisor not found")
}
async fn list(State(s): State<Service>, headers: HeaderMap) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',c.conversation_id,'actor_id',c.actor_id,'project_id',i.project_id,'label',COALESCE(p.label,'Global supervisor'),'enabled',c.enabled,'revision',c.revision,'profile',c.profile,'phase',CASE WHEN r.observed_at_ms IS NULL OR r.observed_at_ms<$1 THEN 'unavailable' WHEN NOT c.enabled AND r.phase NOT IN ('working','stopping','uncertain') THEN 'stopped' ELSE r.phase END,'stage',CASE WHEN r.observed_at_ms IS NULL OR r.observed_at_ms<$1 THEN 'Supervisor service is unavailable' ELSE r.stage END,'observed_at_ms',r.observed_at_ms,'generation',r.generation,'session_id',r.session_id,'last_error',r.last_error) FROM supervisor_controls c JOIN supervisor_conversation_ids i USING(conversation_id) LEFT JOIN project_supervisor_conversations p USING(conversation_id) LEFT JOIN supervisor_service_reports r USING(conversation_id) ORDER BY i.project_id IS NOT NULL,p.label,c.conversation_id")
        .bind(chrono::Utc::now().timestamp_millis()-15000).fetch_all(&s.pool).await?;
    let profiles = aq_manual_sessions::intelligence::choices(&s.pool, &s.choices)
        .await?
        .into_iter()
        .filter(|p| {
            p["harness"] == "codex"
                && matches!(p["reasoning"].as_str(), Some("low" | "medium" | "high"))
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"supervisors":rows,"profiles":profiles,"server_time_ms":chrono::Utc::now().timestamp_millis()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    operation_id: Uuid,
    expected_revision: u64,
    enabled: bool,
    #[serde(default)]
    intelligence_profile: Option<aq_manual_sessions::intelligence::Selection>,
}
async fn control(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(v): Json<Control>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    if id.is_nil() || v.operation_id.is_nil() || v.expected_revision >= i64::MAX as u64 {
        return Err(invalid("Invalid supervisor control identity"));
    }
    let request = json!({"conversation_id":id,"expected_revision":v.expected_revision,"enabled":v.enabled,"intelligence_profile":v.intelligence_profile});
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(109109)")
        .execute(&mut *tx)
        .await?;
    if let Some(old) = sqlx::query(
        "SELECT actor_id,request,receipt FROM supervisor_control_operations WHERE operation_id=$1",
    )
    .bind(v.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if old.get::<Uuid, _>("actor_id") != actor || old.get::<Value, _>("request") != request {
            return Err(conflict("Supervisor control identity is already used"));
        }
        return Ok(Json(old.get("receipt")));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM supervisor_control_operations")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 4096 {
        return Err(conflict("Supervisor control history needs maintenance"));
    }
    let current = sqlx::query(
        "SELECT revision,profile FROM supervisor_controls WHERE conversation_id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    if current.get::<i64, _>("revision") != v.expected_revision as i64 {
        return Err(conflict(
            "Supervisor settings changed; refresh before retrying",
        ));
    }
    let profile = if let Some(selected) = &v.intelligence_profile {
        aq_manual_sessions::intelligence::choices(&s.pool, &s.choices)
            .await?
            .into_iter()
            .find(|p| {
                p["harness"] == "codex"
                    && matches!(p["reasoning"].as_str(), Some("low" | "medium" | "high"))
                    && p["profile_id"] == selected.profile_id
                    && p["profile_version"] == selected.profile_version
                    && p["candidate_id"] == selected.candidate_id
            })
            .ok_or_else(|| {
                conflict("Intelligence profile changed or is unavailable; select it again")
            })?
    } else {
        current
            .get::<Option<Value>, _>("profile")
            .unwrap_or(Value::Null)
    };
    if v.enabled && profile.is_null() {
        return Err(invalid("Choose a supervisor intelligence profile"));
    }
    let at = chrono::Utc::now().timestamp_millis();
    let receipt = json!({"operation_id":v.operation_id,"conversation_id":id,"revision":v.expected_revision+1,"enabled":v.enabled,"profile":profile});
    let approved: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND role='owner' FOR SHARE")
            .bind(actor)
            .fetch_optional(&mut *tx)
            .await?;
    if approved.is_none() {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Current owner authorization is required",
        ));
    }
    sqlx::query("UPDATE supervisor_controls SET enabled=$2,revision=$3,profile=$4,approved_by=$5,updated_at_ms=$6 WHERE conversation_id=$1").bind(id).bind(v.enabled).bind((v.expected_revision+1)as i64).bind(if profile.is_null(){None}else{Some(profile)}).bind(actor).bind(at).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO supervisor_control_operations(operation_id,actor_id,request,receipt,created_at_ms) VALUES($1,$2,$3,$4,$5)").bind(v.operation_id).bind(actor).bind(request).bind(&receipt).bind(at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}
async fn operation(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((id, operation)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    let receipt:Option<Value>=sqlx::query_scalar("SELECT receipt FROM supervisor_control_operations WHERE operation_id=$1 AND actor_id=$2 AND request->>'conversation_id'=$3").bind(operation).bind(actor).bind(id.to_string()).fetch_optional(&s.pool).await?;
    Ok(Json(receipt.ok_or_else(missing)?))
}
