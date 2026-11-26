use super::*;
pub(super) async fn load(r: &ReadRuntime, id: Uuid) -> Result<Option<Operation>> {
    if id.is_nil() {
        return Err(invalid());
    }
    sqlx::query("SELECT * FROM human_graph_operations WHERE operation_id=$1")
        .bind(id)
        .fetch_optional(&r.pool)
        .await
        .map_err(db)?
        .map(Operation::decode)
        .transpose()
}
pub(super) fn owns(op: &Operation, actor: &User) -> Result<()> {
    if op.actor != actor.id {
        return Err(AppError::NotFound("operation not found".into()));
    }
    Ok(())
}
pub(super) async fn transaction(r: &ReadRuntime) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
    let mut tx = r.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='1500ms'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}
async fn owner_locked(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, actor: &User) -> Result<()> {
    let row = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1 FOR SHARE")
        .bind(actor.id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(conflict)?;
    let has_owner_role = row.get::<String, _>("role") == "owner";
    let same_identity = row.get::<String, _>("email") == actor.email
        && row.get::<Option<String>, _>("issuer") == actor.issuer
        && row.get::<Option<String>, _>("subject") == actor.subject;
    if !has_owner_role || !same_identity {
        return Err(conflict());
    }
    Ok(())
}
pub(super) async fn guard(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &User,
    op: &Operation,
) -> Result<Option<String>> {
    owns(op, actor)?;
    owner_locked(tx, actor).await?;
    if op.status.terminal() {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(op.project)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    if row.is_none_or(|row| {
        row.get::<i64, _>("revision") != op.revision || row.get::<Value, _>("binding") != op.binding
    }) {
        return Ok(Some("project_binding_changed".into()));
    }
    let identity = runtime(state).and_then(|rt| {
        super::super::super::mutations::service_identity(rt, op.workspace)
            .map_err(|_| unavailable())
    });
    if identity.ok().as_ref() != Some(&op.service) {
        return Ok(Some("graph_service_changed".into()));
    }
    Ok(op.recovery_blocker(stamp(state)))
}
impl Operation {
    fn recovery_blocker(&self, now: i64) -> Option<String> {
        if now < self.created {
            return Some("clock_changed".into());
        }
        if now >= self.expires || self.count >= MAX_ATTEMPTS {
            return Some("recovery_budget_exhausted".into());
        }
        if let Some(reason) = &self.stop {
            return Some(reason.clone());
        }
        if self.lease_until.is_some_and(|until| until > now) {
            return Some("dispatch_in_progress".into());
        }
        None
    }
}
pub(super) async fn status(
    state: &AppState,
    r: &ReadRuntime,
    actor: &User,
    op: &Operation,
) -> Result<Status> {
    let mut tx = transaction(r).await?;
    let blocked = guard(state, &mut tx, actor, op).await?;
    tx.commit().await.map_err(db)?;
    let mut reply = op.status.clone();
    if reply.terminal() {
        return Ok(reply);
    }
    reply.recovery = Recovery {
        allowed: blocked.is_none(),
        reason: blocked,
    };
    Ok(reply)
}
pub(super) struct Admission<'a> {
    pub record: &'a ProjectRecord,
    pub request: &'a Create,
    pub bytes: &'a [u8],
    pub payload: &'a [u8],
    pub service: &'a Value,
}
impl Admission<'_> {
    pub(super) async fn admit(&self, owner: &Owner<'_>) -> Result<()> {
        let Owner {
            state,
            runtime: r,
            actor,
        } = *owner;
        let now = stamp(state);
        let retained = self.retained_size(now)?;
        let workspace =
            crate::project_bindings::workspace::creation::prepare(state, self.record, self.request)
                .await?;
        let mut tx = transaction(r).await?;
        owner_locked(&mut tx, actor).await?;
        let current = sqlx::query(
            "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
        )
        .bind(self.record.binding.project_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(conflict)?;
        let binding = serde_json::to_value(&self.record.binding).map_err(|_| unavailable())?;
        let stale = current.get::<i64, _>("revision")
            != self.request.revision().ok_or_else(invalid)?
            || current.get::<Value, _>("binding") != binding;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('aq-human-graph-admission-v1',0))",
        )
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if let Some(row) =
            sqlx::query("SELECT * FROM human_graph_operations WHERE operation_id=$1 FOR UPDATE")
                .bind(self.request.operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?
        {
            let old = Operation::decode(row)?;
            owns(&old, actor)?;
            if !old.matches_intent(self.record.binding.project_id, self.bytes)
                || old.payload != self.payload
            {
                return Err(conflict());
            }
        } else {
            ensure_capacity(&mut tx, retained).await?;
            self.insert(
                &mut tx,
                actor,
                JournalSlot {
                    now,
                    stale,
                    binding,
                    retained,
                },
            )
            .await?;
            if !stale {
                crate::project_bindings::workspace::creation::stage(
                    &mut tx,
                    self.record.binding.project_id,
                    self.request.operation_id,
                    &workspace,
                )
                .await?;
            }
        }
        tx.commit().await.map_err(db)?;
        Ok(())
    }
    fn retained_size(&self, now: i64) -> Result<usize> {
        if now <= 0 || self.payload.len() > 131072 {
            return Err(invalid());
        }
        self.bytes
            .len()
            .checked_add(self.payload.len().checked_mul(2).ok_or_else(invalid)?)
            .and_then(|n| n.checked_add(16384))
            .filter(|n| *n <= 262144)
            .ok_or_else(invalid)
    }
    async fn insert(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        actor: &User,
        slot: JournalSlot,
    ) -> Result<()> {
        let Self {
            record,
            request,
            bytes,
            payload,
            service,
        } = *self;
        let digest = hash(self.bytes);
        let mut status = Status::initial(self, actor.id, digest.clone(), slot.now);
        if slot.stale {
            status.state = OperationState::Conflict;
            status.recovery = Recovery {
                allowed: false,
                reason: Some("project_binding_changed_before_dispatch".into()),
            };
        }
        sqlx::query("INSERT INTO human_graph_operations(operation_id,actor_id,project_id,project_revision,project_binding,workspace_id,task_id,request_bytes,request_digest,payload_bytes,payload_digest,service_identity,status,created_at_ms,expires_at_ms,retained_bytes) VALUES($1,$2,$3,$4,$5,$6,$1,$7,$8,$9,$10,$11,$12,$13,$14,$15)")
            .bind(request.operation_id)
            .bind(actor.id)
            .bind(record.binding.project_id)
            .bind(request.revision().ok_or_else(invalid)?)
            .bind(slot.binding)
            .bind(record.binding.graph_workspace_id)
            .bind(bytes)
            .bind(digest)
            .bind(payload)
            .bind(hash(payload))
            .bind(service)
            .bind(serde_json::to_value(status).map_err(|_|unavailable())?)
            .bind(slot.now)
            .bind(slot.now.checked_add(BUDGET).ok_or_else(unavailable)?)
            .bind(slot.retained as i64)
            .execute(&mut **tx)
            .await.map_err(db)?;
        Ok(())
    }
}
struct JournalSlot {
    now: i64,
    stale: bool,
    binding: Value,
    retained: usize,
}
async fn ensure_capacity(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    retained: usize,
) -> Result<()> {
    let total = sqlx::query("SELECT count(*)::bigint AS count,COALESCE(sum(retained_bytes),0)::bigint AS bytes FROM human_graph_operations").fetch_one(&mut **tx).await.map_err(db)?;
    if total.get::<i64, _>("count") >= 4096
        || total
            .get::<i64, _>("bytes")
            .checked_add(retained as i64)
            .is_none_or(|n| n > 64 * 1024 * 1024)
    {
        return Err(unavailable());
    }
    Ok(())
}
