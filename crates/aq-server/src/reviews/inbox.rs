//! Bounded discovery over the existing encrypted review owner. Cursors reveal no
//! foreign decision IDs and bind the actor, filter, project and original expiry.
use super::*;
use serde::{Deserialize, Serialize};
const SCAN: i64 = 64;
const CURSOR_TTL: u64 = 30_000;
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxQuery {
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub state: InboxState,
    #[serde(default = "page_size")]
    pub limit: u32,
    pub cursor: Option<String>,
}
fn page_size() -> u32 {
    25
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    #[default]
    Open,
    Closed,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    actor: Uuid,
    project: Option<Uuid>,
    state: InboxState,
    limit: u32,
    after: Uuid,
    expires: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedCursor {
    version: u32,
    key_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxItem {
    pub decision_id: Uuid,
    pub decision_revision: u64,
    pub state: String,
    pub expires_at_ms: u64,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub report_id: Uuid,
    pub current_report_head: bool,
    pub eligible_owner: bool,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxPage {
    pub api_version: String,
    pub server_time_ms: u64,
    pub items: Vec<InboxItem>,
    pub next_cursor: Option<String>,
    /// Pages sample current authorized rows; they are not a transactional snapshot.
    pub sampled: bool,
}
impl Service {
    fn cursor(&self, cursor: &Cursor) -> Result<String> {
        let sealed = self.seal("inbox-cursor", &cursor.actor.to_string(), &encode(cursor)?)?;
        let wire = ProtectedCursor {
            version: sealed.version,
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        };
        Ok(hex::encode(
            serde_json::to_vec(&wire).map_err(|_| AppError::Internal)?,
        ))
    }
    fn read_cursor(&self, actor: Uuid, query: &InboxQuery, at: u64) -> Result<Option<Cursor>> {
        let Some(token) = &query.cursor else {
            return Ok(None);
        };
        if token.len() > 4096 {
            return Err(invalid());
        }
        let bytes = hex::decode(token).map_err(|_| invalid())?;
        let c: ProtectedCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let bytes = self
            .keys
            .open(
                &Self::aad("inbox-cursor", &actor.to_string()),
                &Sealed {
                    version: c.version,
                    key_id: c.key_id,
                    nonce: c.nonce,
                    ciphertext: c.ciphertext,
                },
            )
            .map_err(|_| invalid())?;
        let c: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if c.actor != actor
            || c.project != query.project_id
            || c.state != query.state
            || c.limit != query.limit
            || c.expires <= at
            || c.expires > at.saturating_add(CURSOR_TTL)
        {
            return Err(invalid());
        }
        Ok(Some(c))
    }
    pub async fn inbox(&self, actor: Uuid, query: &InboxQuery) -> Result<InboxPage> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.inbox_inner(actor, query),
        )
        .await
        .map_err(|_| AppError::IdentityUnavailable("review inbox unavailable".into()))?
    }
    async fn inbox_inner(&self, actor: Uuid, query: &InboxQuery) -> Result<InboxPage> {
        if !(1..=50).contains(&query.limit) || query.project_id.is_some_and(|id| id.is_nil()) {
            return Err(invalid());
        }
        let at = now(&self.state)?;
        let cursor = self.read_cursor(actor, query, at)?;
        let after = cursor.as_ref().map(|c| c.after);
        let expires = cursor.as_ref().map_or(at + CURSOR_TTL, |c| c.expires);
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        let rows = sqlx::query("SELECT d.* FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE ($1::uuid IS NULL OR s.project_id=$1) AND ($2::uuid IS NULL OR d.decision_id>$2) AND ((d.state='pending')=$3) ORDER BY d.decision_id LIMIT $4")
            .bind(query.project_id).bind(after).bind(query.state==InboxState::Open).bind(SCAN+1).fetch_all(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        let mut items = Vec::new();
        let mut through = after;
        let mut consumed = 0;
        for row in rows.iter().take(SCAN as usize) {
            let id: Uuid = row.get("decision_id");
            let candidate: DecisionRequest = self.decode("request", &id.to_string(), row)?;
            candidate.validate().map_err(|_| AppError::Internal)?;
            if hash(&candidate)? != row.get::<String, _>("request_digest") {
                return Err(AppError::Internal);
            }
            if row.get::<Uuid, _>("requester_id") != actor
                && !candidate.eligible_owners.contains(&actor)
            {
                through = Some(id);
                consumed += 1;
                continue;
            }
            let view = match self.inspect(actor, id).await {
                Ok(view) => view,
                Err(AppError::Forbidden(_)) => {
                    through = Some(id);
                    consumed += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let request: DecisionRequest =
                serde_json::from_value(view["request"].clone()).map_err(|_| AppError::Internal)?;
            let subject: ReviewSubject =
                serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
            let state = view["state"].as_str().ok_or(AppError::Internal)?;
            through = Some(id);
            consumed += 1;
            // Current observations can move between filters while a page is read.
            if (state == "pending") != (query.state == InboxState::Open) {
                continue;
            }
            items.push(InboxItem {
                decision_id: id,
                decision_revision: view["decision_revision"]
                    .as_u64()
                    .ok_or(AppError::Internal)?,
                state: state.into(),
                expires_at_ms: request.expires_at_ms,
                project_id: subject.project_id,
                task_id: subject.task.task_id,
                report_id: subject.result.report_id,
                current_report_head: view["current_report_head"]
                    .as_bool()
                    .ok_or(AppError::Internal)?,
                eligible_owner: request.eligible_owners.contains(&actor),
            });
            if items.len() >= query.limit as usize {
                break;
            }
        }
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        tx.commit().await.map_err(db)?;
        let clock = now(&self.state)?;
        if clock < at || clock >= expires {
            return Err(invalid());
        }
        let next_cursor = if consumed < rows.len() {
            Some(self.cursor(&Cursor {
                actor,
                project: query.project_id,
                state: query.state,
                limit: query.limit,
                after: through.ok_or(AppError::Internal)?,
                expires,
            })?)
        } else {
            None
        };
        Ok(InboxPage {
            api_version: "review-inbox-1".into(),
            server_time_ms: clock,
            items,
            next_cursor,
            sampled: true,
        })
    }
}
pub(super) async fn inbox_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Query(query): axum::extract::Query<InboxQuery>,
) -> Result<impl axum::response::IntoResponse> {
    Ok((
        [("cache-control", "no-store")],
        axum::Json(service.inbox(actor.id, &query).await?),
    ))
}

/// Schema generated from the exact human inbox response DTO.
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(InboxPage)).expect("review inbox schema encoding")
}
