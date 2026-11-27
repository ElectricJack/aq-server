use super::*;
use aq_machine_control::management as control;
use axum::{
    Router,
    extract::{Path, State},
    routing::{get, post},
};
use sqlx::Row;

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/api/worker-management", get(list))
        .route("/api/worker-management/{kind}/{id}", post(save).put(save))
}

async fn list(State(s): State<Service>, headers: HeaderMap) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let mut conn = s.pool.acquire().await?;
    let settings = control::settings(&mut conn).await.map_err(storage)?;
    let default: (i32, i64) = sqlx::query_as("SELECT slots,revision FROM node_slot_default")
        .fetch_one(&mut *conn)
        .await?;
    Ok(Json(
        json!({"settings": settings,"node_slot_default":{"slots":default.0,"revision":default.1}}),
    ))
}

pub(super) fn storage(error: aq_machine_control::Error) -> Error {
    match error {
        aq_machine_control::Error::NotFound => missing(),
        _ => Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Worker settings are temporarily unavailable",
        ),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    operation_id: Uuid,
    expected_revision: u64,
    label: Option<String>,
    max_agents: Option<u32>,
    #[serde(default)]
    remove: bool,
}

async fn save(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
    Json(mut change): Json<Change>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(change.operation_id)?;
    let target = Target {
        kind: &kind,
        id: &id,
    };
    change.validate(target)?;
    let request = json!({"kind":kind, "id":id, "change":change});
    let mut tx = s.pool.begin().await?;
    // Fences the nodes the change affects (every node for the default), so
    // publication and claims see it; a repeat of this operation waits here too.
    let nodes = members(&mut tx, target).await?;
    aq_machine_control::allocation::fence(&mut tx, (kind != "default").then_some(&nodes[..]))
        .await
        .map_err(storage)?;
    if let Some(row) = sqlx::query(
        "SELECT actor_id,request,receipt FROM worker_setting_operations WHERE operation_id=$1",
    )
    .bind(change.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Uuid, _>("actor_id") != actor || row.get::<Value, _>("request") != request {
            return Err(conflict(
                "This operation was already used for different settings",
            ));
        }
        return Ok(Json(row.get("receipt")));
    }
    let receipt = if kind == "default" {
        change.save_default(&mut tx, &id).await?
    } else {
        change.save_target(&mut tx, target, &nodes).await?
    };
    sqlx::query("INSERT INTO worker_setting_operations(operation_id,actor_id,request,receipt) VALUES($1,$2,$3,$4)")
        .bind(change.operation_id).bind(actor).bind(request).bind(&receipt).execute(&mut *tx).await?;
    tx.commit().await?;
    if kind == "machine" && change.remove {
        // Retiring a machine revokes its nodes: wait for requests they already started.
        let now = chrono::Utc::now();
        aq_machine_control::drain_revoked(&s.pool, &nodes, now)
            .await
            .map_err(storage)?;
    }
    Ok(Json(receipt))
}

#[derive(Clone, Copy)]
struct Target<'a> {
    kind: &'a str,
    id: &'a str,
}
fn valid_target(Target { kind, id }: Target<'_>) -> bool {
    if kind == "node" {
        id.parse::<Uuid>().is_ok()
    } else if kind == "default" {
        id == "node-slots"
    } else {
        kind == "machine"
            && (aq_machine_control::valid_machine_key(id)
                || id
                    .strip_prefix("node:")
                    .is_some_and(|v| v.parse::<Uuid>().is_ok()))
    }
}
impl Change {
    fn validate(&mut self, target: Target<'_>) -> Result<()> {
        if !valid_target(target) || self.expected_revision >= i64::MAX as u64 {
            return Err(bad("Invalid worker identity or revision"));
        }
        self.label = self
            .label
            .take()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        if self
            .label
            .as_ref()
            .is_some_and(|v| v.chars().count() > 64 || v.chars().any(char::is_control))
        {
            return Err(bad("Names must be at most 64 characters"));
        }
        let maximum = if matches!(target.kind, "node" | "default") {
            1024
        } else {
            i32::MAX as u32
        };
        if self.max_agents.is_some_and(|v| v > maximum) {
            return Err(bad("Agent maximum is outside the supported range"));
        }
        Ok(())
    }
    async fn save_default(&self, conn: &mut sqlx::PgConnection, id: &str) -> Result<Value> {
        if !matches!(
            self,
            Self {
                remove: false,
                label: None,
                max_agents: Some(_),
                ..
            }
        ) {
            return Err(bad("Default requires max_agents only"));
        }
        let changed = sqlx::query(
            "UPDATE node_slot_default SET slots=$1,revision=revision+1 WHERE revision=$2",
        )
        .bind(self.max_agents.map(|v| v as i32))
        .bind(self.expected_revision as i64)
        .execute(&mut *conn)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(conflict("Settings changed. Refresh before saving again."));
        }
        Ok(
            json!({"kind":"default","id":id,"max_agents":self.max_agents,"revision":self.expected_revision+1}),
        )
    }
    async fn save_target(
        &mut self,
        conn: &mut sqlx::PgConnection,
        target: Target<'_>,
        nodes: &[Uuid],
    ) -> Result<Value> {
        let Target { kind, id } = target;
        if nodes.is_empty() {
            return Err(missing());
        }
        let current: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM worker_settings WHERE kind=$1 AND id=$2")
                .bind(kind)
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        if current.unwrap_or(0) as u64 != self.expected_revision {
            return Err(conflict("Settings changed. Refresh before saving again."));
        }
        if kind == "node" && self.max_agents.is_none() {
            self.max_agents = Some(
                aq_machine_control::slots::count(
                    &mut *conn,
                    Uuid::parse_str(id).map_err(|_| bad("Invalid node"))?,
                )
                .await
                .map_err(storage)?,
            );
        }
        let retired = kind == "node" && self.remove;
        let revision = self.expected_revision + 1;
        sqlx::query(
        "INSERT INTO worker_settings(kind,id,label,max_agents,removed,revision) VALUES($1,$2,$3,$4,$5,$6) \
         ON CONFLICT(kind,id) DO UPDATE SET label=EXCLUDED.label,max_agents=EXCLUDED.max_agents,removed=EXCLUDED.removed,revision=EXCLUDED.revision"
    ).bind(kind).bind(id).bind(&self.label).bind(self.max_agents.map(|v| v as i32))
        .bind(retired).bind(revision as i64).execute(&mut *conn).await?;
        if kind == "node"
            && let Some(label) = &self.label
        {
            sqlx::query("UPDATE nodes SET name=$2 WHERE id=ANY($1)")
                .bind(nodes)
                .bind(label)
                .execute(&mut *conn)
                .await?;
        }
        if self.remove {
            retire_nodes(conn, target, nodes).await?;
        }
        Ok(
            json!({"kind":kind,"id":id,"label":self.label,"max_agents":self.max_agents,
        "removed":retired,"revision":revision}),
        )
    }
}
/// The nodes of a node or machine target, in id order (none for the default).
async fn members(
    conn: &mut sqlx::PgConnection,
    Target { kind, id }: Target<'_>,
) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT node_id FROM worker_machine_members WHERE ($1='node' AND node_id::text=$2) OR ($1='machine' AND machine_id=$2) ORDER BY node_id"
    ).bind(kind).bind(id).fetch_all(&mut *conn).await?)
}
async fn retire_nodes(
    conn: &mut sqlx::PgConnection,
    Target { kind, id }: Target<'_>,
    nodes: &[Uuid],
) -> Result<()> {
    // Retiring one execution node keeps the machine's control connection usable.
    sqlx::query(
        "UPDATE manual_agent_sessions SET desired_running=FALSE, \
             status=CASE WHEN status='pending' THEN 'stopped' ELSE 'stopping' END, \
             stage='Worker removed',updated_at_ms=$2, \
             stopped_at_ms=CASE WHEN status='pending' THEN $2 ELSE stopped_at_ms END \
             WHERE node_id=ANY($1) AND stopped_at_ms IS NULL",
    )
    .bind(nodes)
    .bind(now())
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
             SELECT attempt_id,node_id,'owner_stop',now() FROM execution_attempts \
             WHERE node_id=ANY($1) AND holds_task AND kind='profiled_v1' ON CONFLICT DO NOTHING",
    )
    .bind(nodes)
    .execute(&mut *conn)
    .await?;
    if kind == "machine" {
        sqlx::query("UPDATE nodes SET revoked_at=COALESCE(revoked_at,now()) WHERE id=ANY($1)")
            .bind(nodes)
            .execute(&mut *conn)
            .await?;
        sqlx::query("DELETE FROM machine_controls WHERE machine_key=$1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
