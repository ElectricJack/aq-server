use super::*;
pub(super) struct Scheduler {
    slots: Vec<(Uuid, i32)>,
    used: BTreeMap<Uuid, u32>,
    total: u32,
    running: bool,
    max_slots: u32,
}
impl Scheduler {
    pub(super) fn new(
        sessions: &[Session],
        slots: Vec<(Uuid, i32)>,
        poll: &Poll,
        desired: Option<&str>,
    ) -> Self {
        let mut used = BTreeMap::<Uuid, u32>::new();
        let mut total = 0;
        for x in sessions {
            if x.status != "pending" {
                total += 1;
                *used.entry(x.project_id).or_default() += 1;
            }
        }
        Self {
            slots,
            used,
            total,
            running: !poll.busy && desired.is_none_or(|v| v == "running"),
            max_slots: poll.max_slots,
        }
    }
    fn available(
        &self,
        project: Uuid,
        limit: u32,
        capacity: &aq_machine_control::management::Capacity,
    ) -> bool {
        self.running
            && !capacity.full()
            && self.total < capacity.maximum(self.max_slots)
            && self.used.get(&project).copied().unwrap_or(0) < limit
    }
    pub(super) async fn includes(
        &mut self,
        conn: &mut sqlx::PgConnection,
        x: &mut Session,
        node_at: (Uuid, i64),
    ) -> Result<bool> {
        if x.status == "pending" && x.desired_running {
            self.claim(conn, x, node_at).await
        } else {
            Ok(true)
        }
    }
    async fn claim(
        &mut self,
        conn: &mut sqlx::PgConnection,
        x: &mut Session,
        (node, at): (Uuid, i64),
    ) -> Result<bool> {
        let limit = self
            .slots
            .iter()
            .find(|(id, _)| *id == x.project_id)
            .map_or(1, |(_, v)| *v as u32);
        let limits = aq_machine_control::management::capacity(&mut *conn, node)
            .await
            .map_err(management::storage)?;
        if !self.available(x.project_id, limit, &limits) {
            return Ok(false);
        }
        let cell = aq_swarm_settings::admission::pinned_cell(&x.spec);
        if let Some(reason) = aq_swarm_settings::admission::check(&mut *conn, cell.as_ref()).await?
        {
            x.stage = reason.into();
            sqlx::query("UPDATE manual_agent_sessions SET stage=$2,updated_at_ms=$3 WHERE id=$1 AND status='pending'")
                    .bind(x.id).bind(reason).bind(at).execute(&mut *conn).await?;
            return Ok(false);
        }
        let Some(slot) = aq_machine_control::slots::assign_manual(&mut *conn, x.id, node)
            .await
            .map_err(management::storage)?
        else {
            return Ok(false);
        };
        x.spec["node_slot"] = json!(slot);
        sqlx::query("UPDATE manual_agent_sessions SET spec=$2 WHERE id=$1")
            .bind(x.id)
            .bind(&x.spec)
            .execute(&mut *conn)
            .await?;
        self.total += 1;
        *self.used.entry(x.project_id).or_default() += 1;
        x.status = "preparing".into();
        x.stage = "Preparing workspace".into();
        sqlx::query("UPDATE manual_agent_sessions SET status='preparing',stage='Preparing workspace',updated_at_ms=$2 WHERE id=$1 AND status='pending'").bind(x.id).bind(at).execute(&mut *conn).await?;
        Ok(true)
    }
}
