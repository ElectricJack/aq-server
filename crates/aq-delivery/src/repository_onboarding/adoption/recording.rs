use super::*;

pub(super) struct VerifiedAdoption {
    pub(super) binding_revision: i64,
    pub(super) state: Value,
}
pub(super) struct Attempt<'a> {
    pub(super) request: &'a AdoptRequest,
    pub(super) owner: RequestOwner<'a>,
}
impl Attempt<'_> {
    pub(super) async fn inspect(
        &self,
        pool: &PgPool,
    ) -> Result<std::ops::ControlFlow<Value, (i64, Binding)>> {
        let request = self.request;
        let mut tx = pool.begin().await?;
        let current = binding(&mut tx, request.project_id).await?;
        let old = request.existing(&mut tx, self.owner).await?;
        let current = current.ok_or(MissingProject)?;
        current.1.check(request)?;
        if let Some(old) = old {
            return replay(&current.1, &old).map(std::ops::ControlFlow::Break);
        }
        request.check_history(&mut tx).await?;
        tx.commit().await?;
        Ok(std::ops::ControlFlow::Continue(current))
    }
    pub(super) async fn commit(
        &self,
        pool: &PgPool,
        proof: VerifiedAdoption,
        now_ms: impl FnOnce() -> i64 + Send,
    ) -> Result<Value> {
        let request = self.request;
        let mut tx = pool.begin().await?;
        let (current_revision, current) = binding(&mut tx, request.project_id)
            .await?
            .ok_or(MissingProject)?;
        current.check(request)?;
        if let Some(old) = request.existing(&mut tx, self.owner).await? {
            return replay(&current, &old);
        }
        if current_revision != proof.binding_revision {
            return Err(Conflict("project binding changed during adoption"));
        }
        request.check_history(&mut tx).await?;
        let inserted = sqlx::query(
            "INSERT INTO repository_onboardings
             (operation_id,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms)
             VALUES ($1,$2,$3,$4,$5,'bound',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(request.operation_id)
        .bind(self.owner.actor)
        .bind(self.owner.digest)
        .bind(request.project_id)
        .bind(request.repository_id)
        .bind(&proof.state)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let old = request
                .existing(&mut tx, self.owner)
                .await?
                .ok_or(Conflict("operation_id was already used"))?;
            return replay(&current, &old);
        }
        tx.commit().await?;
        Ok(response(&Row {
            actor: self.owner.actor,
            digest: self.owner.digest.into(),
            step: "bound".into(),
            state: proof.state,
        }))
    }
}
