use super::*;
use aq_notifications::policy::Episode;
use sqlx::postgres::PgRow;

type Tx<'a> = Transaction<'a, Postgres>;

async fn replay(tx: &mut Tx<'_>, id: Uuid) -> Result<Option<PgRow>> {
    Ok( sqlx::query(
            "SELECT request_digest,response,request FROM notification_policy_events WHERE source_event_id=$1",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?)
}

struct ActiveEpisode<'a> {
    source: Episode<'a>,
    key: String,
    generation: i64,
    open: bool,
    current: Option<PgRow>,
}
impl<'a> ActiveEpisode<'a> {
    async fn load(tx: &mut Tx<'_>, episode: Episode<'a>) -> Result<Self> {
        let key = episode.key();
        let current = sqlx::query(
            "SELECT generation,open,workspace_id,identity FROM notification_episodes WHERE episode_key=$1 FOR UPDATE",
        )
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await?;
        let (generation, open) = match current.as_ref() {
            Some(row) => {
                if row.get::<Uuid, _>("workspace_id") != episode.event.scope.workspace_id {
                    return Err(Error::Conflict);
                }
                (row.get::<i64, _>("generation"), row.get::<bool, _>("open"))
            }
            None => (0, false),
        };
        Ok(Self {
            source: episode,
            key,
            generation,
            open,
            current,
        })
    }
    async fn apply(&self, tx: &mut Tx<'_>, now: u64) -> Result<(Value, Option<Value>)> {
        let episode = &self.source;
        let key = &self.key;
        let generation = self.generation;
        let open = self.open;
        let event = episode.event;
        let response = match (event.transition, open) {
            (Transition::Opened, true) => {
                json!({"episode_key":key,"generation":generation,"outcome":"already_open"})
            }
            (Transition::Resolved, false) => {
                json!({"episode_key":key,"generation":generation,"outcome":"not_open"})
            }
            (Transition::Resolved, true) => {
                sqlx::query("UPDATE notification_episodes SET open=FALSE,resolved_at_ms=$2 WHERE episode_key=$1")
                    .bind(key).bind(now as i64).execute(&mut **tx).await?;
                json!({"episode_key":key,"generation":generation,"outcome":"resolved"})
            }
            (Transition::Opened, false) => return self.open(tx, now).await,
        };
        Ok((response, None))
    }
    async fn open(&self, tx: &mut Tx<'_>, now: u64) -> Result<(Value, Option<Value>)> {
        let current = &self.current;
        let key = &self.key;
        let event = self.source.event;
        let subject = self.source.subject;
        if current.is_none() && count(tx, "notification_episodes").await? >= MAX_EPISODES {
            return Err(Error::Overloaded);
        }
        let generation = self.generation + 1;
        let identity = current
            .as_ref()
            .map_or(self.source.event.source_event_id, |r| {
                r.get::<Uuid, _>("identity")
            });
        let window = (now / FLOOD_WINDOW_MS * FLOOD_WINDOW_MS) as i64;
        let notified = window_count(tx, &event.scope, window).await?;
        let mut request = None;
        let outcome = if notified < FLOOD_INDIVIDUAL_LIMIT {
            let send = self.source.message(owner(tx).await?, identity, generation);
            request = Some(serde_json::to_value(&send)?);
            sqlx::query("UPDATE notification_flood_windows SET notified=notified+1 WHERE project_id=$1 AND workspace_id=$2 AND window_start_ms=$3")
                        .bind(event.scope.project_id).bind(event.scope.workspace_id).bind(window).execute(&mut **tx).await?;
            "notified"
        } else {
            sqlx::query("UPDATE notification_flood_windows SET collapsed=collapsed+1,kinds=CASE WHEN $4=ANY(kinds) THEN kinds ELSE array_append(kinds,$4) END WHERE project_id=$1 AND workspace_id=$2 AND window_start_ms=$3")
                        .bind(event.scope.project_id).bind(event.scope.workspace_id).bind(window).bind(event.kind.as_str()).execute(&mut **tx).await?;
            "collapsed"
        };
        sqlx::query("INSERT INTO notification_episodes(episode_key,kind,subject,project_id,workspace_id,generation,open,opened_at_ms,outcome,request,identity) VALUES($1,$2,$3,$4,$5,$6,TRUE,$7,$8,$9,$10) ON CONFLICT(episode_key) DO UPDATE SET generation=EXCLUDED.generation,open=TRUE,opened_at_ms=EXCLUDED.opened_at_ms,resolved_at_ms=NULL,outcome=EXCLUDED.outcome,request=EXCLUDED.request")
                    .bind(key).bind(event.kind.as_str()).bind(subject).bind(event.scope.project_id).bind(event.scope.workspace_id).bind(generation).bind(now as i64).bind(outcome).bind(&request).bind(identity).execute(&mut **tx).await?;
        let mut response = json!({"episode_key":key,"generation":generation,"outcome":outcome});
        if let Some(r) = &request {
            response["message_id"] = r["message_id"].clone();
        }
        Ok((response, request))
    }
}

async fn window_count(tx: &mut Tx<'_>, scope: &Scope, window: i64) -> Result<i32> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notification_flood_windows WHERE project_id=$1 AND workspace_id=$2 AND window_start_ms=$3)")
                    .bind(scope.project_id).bind(scope.workspace_id).bind(window).fetch_one(&mut **tx).await?;
    if !exists && count(tx, "notification_flood_windows").await? >= MAX_WINDOWS {
        return Err(Error::Overloaded);
    }
    let notified: i32 = sqlx::query_scalar("INSERT INTO notification_flood_windows(project_id,workspace_id,window_start_ms) VALUES($1,$2,$3) ON CONFLICT(project_id,workspace_id,window_start_ms) DO UPDATE SET notified=notification_flood_windows.notified RETURNING notified")
                    .bind(scope.project_id).bind(scope.workspace_id).bind(window).fetch_one(&mut **tx).await?;
    Ok(notified)
}

impl Policy {
    /// Records one source observation and submits the notification it makes
    /// due, if any. Replaying a source event returns the same outcome.
    pub async fn observe(&self, event: Event) -> Result<Value> {
        let subject = event.subject_key().ok_or(Error::Invalid)?;
        let hash = crate::notifications::persistence::digest(&("policy-event/1", &event))?;
        let mut tx = self.service.pool.begin().await?;
        lock(&mut tx).await?;
        let replay = replay(&mut tx, event.source_event_id).await?;
        if let Some(row) = replay {
            tx.commit().await?;
            if row.get::<String, _>("request_digest") != hash {
                return Err(Error::Conflict);
            }
            return self
                .complete(
                    event.source_event_id,
                    row.get("request"),
                    row.get("response"),
                )
                .await;
        }
        if count(&mut tx, "notification_policy_events").await? >= MAX_EVENTS {
            return Err(Error::Overloaded);
        }
        let now = self.service.now()?;
        let episode = ActiveEpisode::load(
            &mut tx,
            Episode {
                event: &event,
                subject: &subject,
            },
        )
        .await?;
        let (response, request) = episode.apply(&mut tx, now).await?;
        sqlx::query("INSERT INTO notification_policy_events(source_event_id,request_digest,response,request,created_at_ms,submitted) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(event.source_event_id).bind(&hash).bind(&response).bind(&request).bind(now as i64).bind(request.is_none()).execute(&mut *tx).await?;
        tx.commit().await?;
        self.complete(event.source_event_id, request, response)
            .await
    }
    async fn complete(&self, id: Uuid, request: Option<Value>, response: Value) -> Result<Value> {
        let response = self.submit(request, response).await?;
        sqlx::query(
            "UPDATE notification_policy_events SET submitted=TRUE WHERE source_event_id=$1",
        )
        .bind(id)
        .execute(&self.service.pool)
        .await?;
        Ok(response)
    }
}
