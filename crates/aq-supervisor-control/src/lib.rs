//! Central supervisor scopes and operator controls, separate from worker sessions.
pub mod admin;
mod api;
pub mod attachments;
pub mod authority;
pub mod delivery;
pub mod extract;
#[cfg(all(feature = "delivery_playbook", feature = "supervisor_full_access"))]
pub mod git;
#[cfg(feature = "supervisor_full_access")]
pub mod owner;
pub mod read;
pub mod wire;
mod zip;
pub use api::{Auth, Service};
pub use aq_manual_sessions::intelligence::resolve_stored;
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
pub use wire::Conversation;
/// The supervisor routes on the machine listener: `/v1/supervisor/projects`,
/// and with supervisor_full_access the owner bridge to `owner_app`; without
/// it, the read nest over `human` and the admin nest over `settings`.
pub fn machine_routes<S: Clone + Send + Sync + 'static>(
    human: axum::Router<S>,
    settings: axum::Router<S>,
    owner_app: Option<axum::Router>,
    pool: sqlx::PgPool,
    clock: Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
) -> axum::Router<S> {
    let clock: read::Clock = Arc::new(move || clock().timestamp_millis() as u64);
    let projects = read::projects(pool.clone(), clock.clone());
    #[cfg(feature = "supervisor_full_access")]
    {
        let _ = (human, settings);
        let bridge = owner_app.map(|app| owner::routes(app, pool, clock));
        projects.merge(bridge.unwrap_or_default())
    }
    #[cfg(not(feature = "supervisor_full_access"))]
    {
        let _ = owner_app;
        projects
            .merge(read::routes(human, pool.clone(), clock.clone()))
            .merge(admin::routes(settings, pool, clock))
    }
}
/// The supervisor git route `/v1/supervisor/git` on the machine listener,
/// forwarding fetches to aq-git's git `socket`: mounted only with
/// supervisor_full_access and a configured socket.
#[cfg(feature = "delivery_playbook")]
pub fn git_routes<S: Clone + Send + Sync + 'static>(
    pool: &sqlx::PgPool,
    clock: &Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
    socket: Option<&std::path::Path>,
) -> axum::Router<S> {
    #[cfg(feature = "supervisor_full_access")]
    if let Some(socket) = socket {
        let clock = clock.clone();
        let clock: read::Clock = Arc::new(move || clock().timestamp_millis() as u64);
        return git::routes(pool.clone(), clock, socket.to_owned());
    }
    let _ = (pool, clock, socket);
    axum::Router::new()
}
/// Scope identity is immutable; the caller supplies owner authorization. These
/// are the original global locks/counters, or the project's distinct locks.
pub async fn definition(
    tx: &mut Transaction<'_, Postgres>,
    id: Option<Uuid>,
    write: bool,
) -> Result<Conversation, sqlx::Error> {
    let global: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await?;
    let actual = id.unwrap_or(global);
    let sql = if actual == global {
        if write {
            "SELECT conversation_id,revision,'Global supervisor'::text AS label FROM global_supervisor_conversation WHERE conversation_id=$1 FOR UPDATE"
        } else {
            "SELECT conversation_id,revision,'Global supervisor'::text AS label FROM global_supervisor_conversation WHERE conversation_id=$1 FOR SHARE"
        }
    } else if write {
        "SELECT conversation_id,revision,label FROM project_supervisor_conversations WHERE conversation_id=$1 FOR UPDATE"
    } else {
        "SELECT conversation_id,revision,label FROM project_supervisor_conversations WHERE conversation_id=$1 FOR SHARE"
    };
    let row = sqlx::query(sql).bind(actual).fetch_one(&mut **tx).await?;
    Ok(Conversation {
        id: row.get("conversation_id"),
        revision: row.get::<i64, _>("revision").to_string(),
        label: row.get("label"),
    })
}
pub async fn charge(
    tx: &mut Transaction<'_, Postgres>,
    conversation: Uuid,
    bytes: i64,
    max_ops: i64,
    max_bytes: i64,
) -> Result<bool, sqlx::Error> {
    let global: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await?;
    let project = global != conversation;
    let sql = if project {
        "UPDATE project_supervisor_conversations SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$2 WHERE conversation_id=$1 AND retained_operations<$3 AND retained_bytes<=$4-$2"
    } else {
        "UPDATE global_supervisor_conversation SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$2 WHERE conversation_id=$1 AND retained_operations<$3 AND retained_bytes<=$4-$2"
    };
    Ok(sqlx::query(sql)
        .bind(conversation)
        .bind(bytes)
        .bind(max_ops)
        .bind(max_bytes)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        == 1)
}
pub async fn project(
    tx: &mut Transaction<'_, Postgres>,
    conversation: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT project_id FROM supervisor_conversation_ids WHERE conversation_id=$1",
    )
    .bind(conversation)
    .fetch_one(&mut **tx)
    .await
}

/// The controller is alive and accepting mail, including between physical turns.
pub async fn connected(pool: &sqlx::PgPool, conversation: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM supervisor_controls c JOIN supervisor_service_reports r USING(conversation_id) WHERE c.conversation_id=$1 AND c.enabled AND r.observed_at_ms>$2 AND r.phase IN ('idle','working','waiting'))")
        .bind(conversation).bind(chrono::Utc::now().timestamp_millis()-15000).fetch_one(pool).await.unwrap_or(false)
}
/// Bounded prior context is frozen with the new dispatch, never reserved again.
pub async fn history(
    tx: &mut Transaction<'_, Postgres>,
    conversation: Uuid,
    before: i64,
) -> Result<Vec<u8>, sqlx::Error> {
    let rows=sqlx::query("SELECT body,who FROM (SELECT m.body,'Human'::text AS who,m.cursor AS anchor,0::bigint AS ordinal FROM mailbox_messages m WHERE m.thread_id=$1 AND m.sender->>'kind'='owner' AND m.cursor<$2 UNION ALL SELECT m.body,'Supervisor'::text,d.anchor,m.cursor FROM central_supervisor_replies r JOIN mailbox_messages m ON m.message_id=r.reply_message_id JOIN (SELECT d.dispatch_id,max(mm.cursor) AS anchor FROM central_supervisor_dispatches d JOIN central_supervisor_mail_members mm ON mm.reservation_id=d.reservation_id GROUP BY d.dispatch_id) d ON d.dispatch_id=r.dispatch_id WHERE m.thread_id=$1 AND d.anchor<$2) h ORDER BY anchor DESC,ordinal DESC LIMIT 32").bind(conversation).bind(before).fetch_all(&mut **tx).await?;
    let mut items = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let item = format!(
            "--- prior {} ---\n{}\n",
            row.get::<String, _>("who"),
            row.get::<String, _>("body")
        );
        if bytes + item.len() > 65536 {
            break;
        }
        bytes += item.len();
        items.push(item);
    }
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut text = String::from(
        "Prior conversation context (already handled; do not replay these requests):\n",
    );
    for item in items.into_iter().rev() {
        text.push_str(&item);
    }
    Ok(text.into_bytes())
}

/// Agent completion/blocker mail may target Global or the sending project.
pub async fn admit_mail(
    tx: &mut Transaction<'_, Postgres>,
    conversation: Uuid,
    sender_project: Uuid,
    bytes: i64,
) -> Result<Option<bool>, sqlx::Error> {
    definition(tx, Some(conversation), true).await?;
    let managed: bool =
        sqlx::query_scalar("SELECT to_regclass('supervisor_conversation_ids') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    if managed
        && project(tx, conversation)
            .await?
            .is_some_and(|p| p != sender_project)
    {
        return Ok(None);
    }
    charge(tx, conversation, bytes, 4096, 64 * 1024 * 1024)
        .await
        .map(Some)
}
