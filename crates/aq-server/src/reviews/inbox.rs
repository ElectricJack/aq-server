//! Bounded discovery over the existing encrypted review owner. Cursors reveal no
//! foreign decision IDs and bind the actor, filter, project and original expiry.
use super::*;
use serde::{Deserialize, Serialize};
const SCAN: i64 = 64;
/// Bound on the pending-row scan behind `open_total` and impact ordering.
const OPEN_SCAN: i64 = 256;
/// Bound on distinct graph reads per impact-ordered page.
const BRIEFS: usize = 64;
const CURSOR_TTL: u64 = 30_000;
#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxQuery {
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub state: InboxState,
    /// `impact` requires `state=open`.
    #[serde(default)]
    pub order: InboxOrder,
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
    #[serde(default)]
    order: InboxOrder,
    #[serde(default)]
    offset: u32,
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
    #[schemars(regex(pattern = "^(pending|answered|expired|cancelled)$"))]
    pub state: String,
    pub expires_at_ms: u64,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub report_id: Uuid,
    pub current_report_head: bool,
    pub eligible_owner: bool,
    pub kind: DecisionKind,
    /// Graph task title; null when the graph is unavailable to this read.
    #[schemars(length(max = 4096))]
    pub title: Option<String>,
    #[schemars(length(max = 1024))]
    pub requested_judgment: String,
    pub kind_payload: ReviewPayload,
    pub requested_by: RequestedBy,
    /// Null for decisions admitted before submission times were recorded.
    pub submitted_at_ms: Option<u64>,
    /// Null when the graph is unavailable to this read.
    pub impact: Option<Impact>,
    /// This owner's active deferral, if any.
    pub deferred_until_ms: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxPage {
    #[schemars(regex(pattern = "^review-inbox-1$"))]
    pub api_version: String,
    pub server_time_ms: u64,
    #[schemars(length(max = 50))]
    pub items: Vec<InboxItem>,
    #[schemars(length(max = 4096))]
    pub next_cursor: Option<String>,
    /// Pages sample current authorized rows; they are not a transactional snapshot.
    pub sampled: bool,
    /// Pending decisions requested by or awaiting this owner (in the project
    /// filter), or null when more than the bounded scan exists.
    pub open_total: Option<u64>,
    pub order: InboxOrder,
    /// False when impact ordering ran on partial data (scan bound reached or
    /// some impacts unavailable); such items keep submitted-time order.
    pub order_complete: bool,
}
/// One authorized pending row from the bounded open scan.
struct Open {
    id: Uuid,
    project: Uuid,
    task: Uuid,
    submitted: Option<u64>,
    deferred: bool,
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
            || c.order != query.order
            || c.expires <= at
            || c.expires > at.saturating_add(CURSOR_TTL)
        {
            return Err(invalid());
        }
        Ok(Some(c))
    }
    pub async fn inbox(&self, actor: Uuid, query: &InboxQuery) -> Result<InboxPage> {
        self.inbox_bounded(actor, query, true).await
    }
    /// Inbox page without graph enrichment (title/impact null), for aggregate
    /// readers that already bound their own graph reads.
    pub async fn inbox_lean(&self, actor: Uuid, query: &InboxQuery) -> Result<InboxPage> {
        self.inbox_bounded(actor, query, false).await
    }
    async fn inbox_bounded(
        &self,
        actor: Uuid,
        query: &InboxQuery,
        graph: bool,
    ) -> Result<InboxPage> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.inbox_inner(actor, query, graph),
        )
        .await
        .map_err(|_| AppError::IdentityUnavailable("review inbox unavailable".into()))?
    }
    /// Bounded scan of pending rows this actor requested or may answer.
    /// Returns the rows and whether the scan saw every pending row.
    async fn open_rows(
        &self,
        actor: Uuid,
        project: Option<Uuid>,
        at: u64,
    ) -> Result<(Vec<Open>, bool)> {
        let rows = sqlx::query("SELECT d.decision_id,d.requester_id,d.request_digest,t.submitted_at_ms,d.sealed_version,d.key_id,d.nonce,d.ciphertext,s.project_id,s.task_id,f.deferred_until_ms FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) LEFT JOIN human_decision_submissions t ON t.decision_id=d.decision_id LEFT JOIN human_decision_deferrals f ON f.decision_id=d.decision_id AND f.actor_id=$2 WHERE s.subject_profile='code_review_v1' AND ($1::uuid IS NULL OR s.project_id=$1) AND d.state='pending' ORDER BY d.decision_id LIMIT $3")
            .bind(project).bind(actor).bind(OPEN_SCAN + 1).fetch_all(&self.state.store.pool).await.map_err(db)?;
        let complete = rows.len() as i64 <= OPEN_SCAN;
        let mut out = Vec::new();
        for row in rows.iter().take(OPEN_SCAN as usize) {
            let id: Uuid = row.get("decision_id");
            let request: DecisionRequest = self.decode("request", &id.to_string(), row)?;
            if hash(&request)? != row.get::<String, _>("request_digest") {
                return Err(AppError::Internal);
            }
            if row.get::<Uuid, _>("requester_id") != actor
                && !request.eligible_owners.contains(&actor)
            {
                continue;
            }
            let until: Option<i64> = row.get("deferred_until_ms");
            out.push(Open {
                id,
                project: row.get("project_id"),
                task: row.get("task_id"),
                submitted: row
                    .get::<Option<i64>, _>("submitted_at_ms")
                    .and_then(|v| u64::try_from(v).ok()),
                deferred: until.is_some_and(|u| u64::try_from(u).is_ok_and(|u| u > at)),
            });
        }
        Ok((out, complete))
    }
    async fn item(
        &self,
        actor: Uuid,
        id: Uuid,
        view: &serde_json::Value,
        briefs: &mut view::Briefs,
        at: u64,
    ) -> Result<InboxItem> {
        let request: DecisionRequest =
            serde_json::from_value(view["request"].clone()).map_err(|_| AppError::Internal)?;
        let subject: ReviewSubject =
            serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
        let e = self.enrich(actor, view, briefs, at).await?;
        Ok(InboxItem {
            decision_id: id,
            decision_revision: view["decision_revision"]
                .as_u64()
                .ok_or(AppError::Internal)?,
            state: view["state"].as_str().ok_or(AppError::Internal)?.into(),
            expires_at_ms: request.expires_at_ms,
            project_id: subject.project_id,
            task_id: subject.task.task_id,
            report_id: subject.result.report_id,
            current_report_head: view["current_report_head"]
                .as_bool()
                .ok_or(AppError::Internal)?,
            eligible_owner: request.eligible_owners.contains(&actor),
            kind: e.kind,
            title: e.title,
            requested_judgment: e.requested_judgment,
            kind_payload: e.kind_payload,
            requested_by: e.requested_by,
            submitted_at_ms: e.submitted_at_ms,
            impact: e.impact,
            deferred_until_ms: e.deferred_until_ms,
        })
    }
    /// Impact order over the bounded open scan, paged by offset.
    async fn impact_page(
        &self,
        actor: Uuid,
        query: &InboxQuery,
        cursor: Option<&Cursor>,
        at: u64,
        expires: u64,
        briefs: &mut view::Briefs,
    ) -> Result<InboxPage> {
        let (open, complete) = self.open_rows(actor, query.project_id, at).await?;
        let mut ranked = Vec::with_capacity(open.len());
        let mut capped = false;
        for o in open {
            let known = briefs.lookups() < BRIEFS;
            capped |= !known;
            let reach = if known {
                briefs.get(o.project, o.task).await.map(|b| b.blocked_tasks)
            } else {
                None
            };
            ranked.push((o, reach));
        }
        ranked.sort_by(|(a, ra), (b, rb)| {
            rb.cmp(ra)
                .then(a.deferred.cmp(&b.deferred))
                .then(a.submitted.is_none().cmp(&b.submitted.is_none()))
                .then(a.submitted.cmp(&b.submitted))
                .then(a.id.cmp(&b.id))
        });
        let offset = cursor.map_or(0, |c| c.offset) as usize;
        let mut items = Vec::new();
        for (o, _) in ranked.iter().skip(offset).take(query.limit as usize) {
            let view = match self.inspect(actor, o.id).await {
                Ok(view) => view,
                Err(AppError::Forbidden(_)) => continue,
                Err(error) => return Err(error),
            };
            if view["state"] != "pending" {
                continue;
            }
            items.push(self.item(actor, o.id, &view, briefs, at).await?);
        }
        let end = offset + query.limit as usize;
        let clock = self.page_clock(actor, at, expires).await?;
        let next_cursor = if end < ranked.len() {
            Some(self.cursor(&Cursor {
                actor,
                project: query.project_id,
                state: query.state,
                limit: query.limit,
                after: Uuid::nil(),
                expires,
                order: InboxOrder::Impact,
                offset: u32::try_from(end).map_err(|_| AppError::Internal)?,
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
            open_total: complete.then_some(ranked.len() as u64),
            order: InboxOrder::Impact,
            order_complete: complete && !capped && !briefs.missed,
        })
    }
    /// Re-fence ownership after the reads and bind the page to cursor time.
    async fn page_clock(&self, actor: Uuid, at: u64, expires: u64) -> Result<u64> {
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        tx.commit().await.map_err(db)?;
        let clock = now(&self.state)?;
        if clock < at || clock >= expires {
            return Err(invalid());
        }
        Ok(clock)
    }
    async fn inbox_inner(&self, actor: Uuid, query: &InboxQuery, graph: bool) -> Result<InboxPage> {
        if !(1..=50).contains(&query.limit)
            || query.project_id.is_some_and(|id| id.is_nil())
            || (query.order == InboxOrder::Impact && query.state != InboxState::Open)
        {
            return Err(invalid());
        }
        let at = now(&self.state)?;
        let cursor = self.read_cursor(actor, query, at)?;
        let mut briefs = view::Briefs::new(&self.state, actor, graph).await?;
        if query.order == InboxOrder::Impact {
            let expires = cursor.as_ref().map_or(at + CURSOR_TTL, |c| c.expires);
            {
                let mut tx = self.tx().await?;
                Self::owner(&mut tx, actor).await?;
                tx.commit().await.map_err(db)?;
            }
            return self
                .impact_page(actor, query, cursor.as_ref(), at, expires, &mut briefs)
                .await;
        }
        let after = cursor.as_ref().map(|c| c.after);
        let expires = cursor.as_ref().map_or(at + CURSOR_TTL, |c| c.expires);
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        let rows = sqlx::query("SELECT d.* FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE s.subject_profile='code_review_v1' AND ($1::uuid IS NULL OR s.project_id=$1) AND ($2::uuid IS NULL OR d.decision_id>$2) AND ((d.state='pending')=$3) ORDER BY d.decision_id LIMIT $4")
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
            let state = view["state"].as_str().ok_or(AppError::Internal)?;
            through = Some(id);
            consumed += 1;
            // Current observations can move between filters while a page is read.
            if (state == "pending") != (query.state == InboxState::Open) {
                continue;
            }
            items.push(self.item(actor, id, &view, &mut briefs, at).await?);
            if items.len() >= query.limit as usize {
                break;
            }
        }
        let clock = self.page_clock(actor, at, expires).await?;
        let open_total = if query.state == InboxState::Open {
            let (open, complete) = self.open_rows(actor, query.project_id, at).await?;
            complete.then_some(open.len() as u64)
        } else {
            None
        };
        let next_cursor = if consumed < rows.len() {
            Some(self.cursor(&Cursor {
                actor,
                project: query.project_id,
                state: query.state,
                limit: query.limit,
                after: through.ok_or(AppError::Internal)?,
                expires,
                order: InboxOrder::Decision,
                offset: 0,
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
            open_total,
            order: InboxOrder::Decision,
            order_complete: true,
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
