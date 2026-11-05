//! X6: one read cursor per owner principal over the merged conversation order
//! (see [`reads::Position`]). It only moves forward and never past the latest
//! item; unread counts items after it that the principal did not author.
use super::*;
use reads::{Position, position, render};

const UNREAD_CAP: i64 = 100;
const REPLIES: &str = "SELECT x.anchor,x.cursor FROM (SELECT m.cursor,(SELECT max(mm.cursor) FROM central_supervisor_mail_members mm WHERE mm.actor_id=d.actor_id AND mm.reservation_id=d.reservation_id) AS anchor FROM central_supervisor_replies r JOIN central_supervisor_dispatches d ON d.dispatch_id=r.dispatch_id JOIN mailbox_messages m ON m.message_id=r.reply_message_id WHERE m.mailbox_key=$1) x WHERE x.anchor>0";

#[derive(Serialize)]
struct Unread {
    count: u8,
    capped: bool,
}
#[derive(Serialize)]
pub(super) struct Marker {
    contract: &'static str,
    conversation_id: Uuid,
    principal_id: Uuid,
    read_cursor: Option<String>,
    latest_cursor: Option<String>,
    unread: Unread,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    cursor: String,
}

async fn latest(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<Option<Position>> {
    let human: Option<i64> = sqlx::query_scalar(
        "SELECT max(cursor) FROM mailbox_messages WHERE mailbox_key=$1 AND sender->>'kind'='owner'",
    )
    .bind(key(id)?)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let reply: Option<(i64, i64)> = sqlx::query_as(&format!(
        "{REPLIES} ORDER BY x.anchor DESC,x.cursor DESC LIMIT 1"
    ))
    .bind(digest(&reply::reply_recipient(id)).map_err(|_| unavailable())?)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    Ok(human.map(|c| (c, 0)).max(reply))
}

async fn view(tx: &mut Transaction<'_, Postgres>, user: &User, id: Uuid) -> Result<Marker> {
    let read: Option<(i64, i64)> = sqlx::query_as(
        "SELECT anchor,reply FROM global_supervisor_read_markers WHERE principal_id=$1 AND conversation_id=$2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let (anchor, at) = read.unwrap_or((0, 0));
    let humans: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM mailbox_messages WHERE mailbox_key=$1 AND (cursor,0::bigint)>($2,$3) AND sender<>$4 AND sender->>'kind'='owner' LIMIT $5) t")
        .bind(key(id)?)
        .bind(anchor)
        .bind(at)
        .bind(author(user.id))
        .bind(UNREAD_CAP + 1)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let replies: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM ({REPLIES} AND (x.anchor,x.cursor)>($2,$3) LIMIT $4) t"
    ))
    .bind(digest(&reply::reply_recipient(id)).map_err(|_| unavailable())?)
    .bind(anchor)
    .bind(at)
    .bind(UNREAD_CAP + 1)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let total = humans + replies;
    Ok(Marker {
        contract: CONTRACT,
        conversation_id: id,
        principal_id: user.id,
        read_cursor: read.map(render),
        latest_cursor: latest(tx, id).await?.map(render),
        unread: Unread {
            count: u8::try_from(total.min(UNREAD_CAP)).map_err(|_| unavailable())?,
            capped: total > UNREAD_CAP,
        },
    })
}

pub(super) async fn read(
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(id): Path<Uuid>,
) -> Result<Json<Marker>> {
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    definition(&mut tx, Some(id), false).await?;
    let marker = view(&mut tx, &user, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(marker))
}

pub(super) async fn write(
    State(state): State<AppState>,
    Extension(service): Extension<Arc<Service>>,
    RequireOwner(user): RequireOwner,
    Path(id): Path<Uuid>,
    bytes: Bytes,
) -> Result<Json<Marker>> {
    if bytes.len() > 256 {
        return Err(invalid());
    }
    let update: Update = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let (anchor, at) = position(&update.cursor, false)?;
    let now = i64::try_from(stamp(&state)?).map_err(|_| unavailable())?;
    let mut tx = service.tx().await?;
    owner(&mut tx, &user).await?;
    definition(&mut tx, Some(id), false).await?;
    if latest(&mut tx, id).await?.is_none_or(|l| (anchor, at) > l) {
        return Err(invalid());
    }
    sqlx::query("INSERT INTO global_supervisor_read_markers(principal_id,conversation_id,anchor,reply,updated_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(principal_id,conversation_id) DO UPDATE SET anchor=EXCLUDED.anchor,reply=EXCLUDED.reply,updated_at_ms=EXCLUDED.updated_at_ms WHERE (global_supervisor_read_markers.anchor,global_supervisor_read_markers.reply)<(EXCLUDED.anchor,EXCLUDED.reply)")
        .bind(user.id)
        .bind(id)
        .bind(anchor)
        .bind(at)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let marker = view(&mut tx, &user, id).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(marker))
}
