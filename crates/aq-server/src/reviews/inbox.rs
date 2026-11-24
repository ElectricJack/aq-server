//! Bounded discovery over the existing encrypted review owner. Cursors reveal no
//! foreign decision IDs and bind the actor, filter, project and original expiry.
use super::*;
use aq_dashboard_contract::review_view::{OpenReview as Open, ReviewDetail, order_by_impact};
use serde::{Deserialize, Serialize};
const SCAN: i64 = 64;
/// Bound on the pending-row scan behind `open_total` and impact ordering.
const OPEN_SCAN: i64 = 256;
/// Bound on distinct graph reads per impact-ordered page.
const BRIEFS: usize = 64;
const CURSOR_TTL: u64 = 30_000;
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
        c.validate(actor, query, at)?;
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
        id: Uuid,
        view: &serde_json::Value,
        briefs: &mut view::Briefs,
    ) -> Result<InboxItem> {
        let decision = ReviewDetail::read(view).map_err(|_| AppError::Internal)?;
        let e = self.enrich(view, briefs).await?;
        decision
            .inbox_item(id, briefs.reader, e)
            .ok_or(AppError::Internal)
    }
    /// Impact order over the bounded open scan, paged by offset.
    async fn impact_page(
        &self,
        query: &InboxQuery,
        continuation: Continuation<'_>,
        briefs: &mut view::Briefs,
    ) -> Result<InboxPage> {
        let (actor, at) = (briefs.reader, briefs.at);
        let Continuation { cursor, expires } = continuation;
        let (open, complete) = self.open_rows(actor, query.project_id, at).await?;
        let (ranked, capped) = rank_open(open, briefs).await;
        let offset = cursor.map_or(0, |c| c.offset) as usize;
        let items = self
            .impact_items(
                ranked.iter().skip(offset).take(query.limit as usize),
                briefs,
            )
            .await?;
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
    async fn impact_items<'a>(
        &self,
        ranked: impl Iterator<Item = &'a (Open, Option<u32>)>,
        briefs: &mut view::Briefs,
    ) -> Result<Vec<InboxItem>> {
        let mut items = Vec::new();
        for (o, _) in ranked {
            let view = match self.inspect(briefs.reader, o.id).await {
                Ok(view) => view,
                Err(AppError::Forbidden(_)) => continue,
                Err(error) => return Err(error),
            };
            if view["state"] != "pending" {
                continue;
            }
            items.push(self.item(o.id, &view, briefs).await?);
        }
        Ok(items)
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
        if !(1..=50).contains(&query.limit) || query.project_id.is_some_and(|id| id.is_nil()) {
            return Err(invalid());
        }
        if query.order == InboxOrder::Impact && query.state != InboxState::Open {
            return Err(invalid());
        }
        let at = now(&self.state)?;
        let cursor = self.read_cursor(actor, query, at)?;
        let mut briefs = view::Briefs::new(&self.state, actor, graph, at).await?;
        let expires = cursor.as_ref().map_or(at + CURSOR_TTL, |c| c.expires);
        let continuation = Continuation {
            cursor: cursor.as_ref(),
            expires,
        };
        if query.order == InboxOrder::Impact {
            {
                let mut tx = self.tx().await?;
                Self::owner(&mut tx, actor).await?;
                tx.commit().await.map_err(db)?;
            }
            return self.impact_page(query, continuation, &mut briefs).await;
        }
        self.decision_page(query, continuation, &mut briefs).await
    }

    async fn decision_page(
        &self,
        query: &InboxQuery,
        continuation: Continuation<'_>,
        briefs: &mut view::Briefs,
    ) -> Result<InboxPage> {
        let (actor, at) = (briefs.reader, briefs.at);
        let Continuation { cursor, expires } = continuation;
        let after = cursor.map(|c| c.after);
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
            through = Some(id);
            consumed += 1;
            if let Some(item) = self.decision_item(row, query.state, briefs).await? {
                items.push(item);
            }
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
    async fn decision_item(
        &self,
        row: &sqlx::postgres::PgRow,
        state: InboxState,
        briefs: &mut view::Briefs,
    ) -> Result<Option<InboxItem>> {
        let actor = briefs.reader;
        let id: Uuid = row.get("decision_id");
        let candidate: DecisionRequest = self.decode("request", &id.to_string(), row)?;
        candidate.validate().map_err(|_| AppError::Internal)?;
        if hash(&candidate)? != row.get::<String, _>("request_digest") {
            return Err(AppError::Internal);
        }
        if row.get::<Uuid, _>("requester_id") != actor
            && !candidate.eligible_owners.contains(&actor)
        {
            return Ok(None);
        }
        let view = match self.inspect(actor, id).await {
            Ok(view) => view,
            Err(AppError::Forbidden(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        // Current observations can move between filters while a page is read.
        let observed = view["state"].as_str().ok_or(AppError::Internal)?;
        if (observed == "pending") != (state == InboxState::Open) {
            return Ok(None);
        }
        self.item(id, &view, briefs).await.map(Some)
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

struct Continuation<'a> {
    cursor: Option<&'a Cursor>,
    expires: u64,
}
async fn rank_open(open: Vec<Open>, briefs: &mut view::Briefs) -> (Vec<(Open, Option<u32>)>, bool) {
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
    order_by_impact(&mut ranked);
    (ranked, capped)
}

impl Cursor {
    fn validate(&self, actor: Uuid, query: &InboxQuery, at: u64) -> Result<()> {
        let same_reader = self.actor == actor && self.project == query.project_id;
        let same_filter =
            self.state == query.state && self.limit == query.limit && self.order == query.order;
        if !same_reader || !same_filter {
            return Err(invalid());
        }
        if self.expires <= at || self.expires > at.saturating_add(CURSOR_TTL) {
            return Err(invalid());
        }
        Ok(())
    }
}
