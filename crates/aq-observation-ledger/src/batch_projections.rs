//! Projection rows locked in deterministic order for one observation batch.
use super::{
    Error, ObservationRecord, Postgres, Projection, Result, Transaction, Uuid, Value, parse, value,
};
use std::collections::{BTreeMap, BTreeSet};

pub struct BatchProjections {
    pub(crate) by_lineage: BTreeMap<Uuid, Projection>,
    pub(crate) by_attempt: BTreeMap<Uuid, Projection>,
    current_attempts: BTreeSet<Uuid>,
    pub(crate) updated_attempts: BTreeSet<Uuid>,
}

impl BatchProjections {
    pub(crate) async fn lock(
        tx: &mut Transaction<'_, Postgres>,
        events: &[&ObservationRecord],
        keys: &BTreeSet<String>,
    ) -> Result<Self> {
        let current_attempts: BTreeSet<_> =
            events.iter().map(|e| e.body.binding.attempt_id).collect();
        let affected_attempts = lock_call_owners(tx, keys, &current_attempts).await?;
        let updated_attempts = current_attempts.clone();
        let lineages: BTreeSet<_> = events.iter().map(|e| e.body.binding.lineage_id).collect();
        let mut projections = BTreeMap::new();
        for lineage in lineages {
            sqlx::query("INSERT INTO observation_projections(lineage_id,projection) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(lineage).bind(value(&Projection::default())?).execute(&mut **tx).await?;
            let p: Value = sqlx::query_scalar(
                "SELECT projection FROM observation_projections WHERE lineage_id=$1 FOR UPDATE",
            )
            .bind(lineage)
            .fetch_one(&mut **tx)
            .await?;
            projections.insert(lineage, parse::<Projection>(p)?);
        }
        let mut attempt_projections = BTreeMap::new();
        for attempt in affected_attempts {
            if current_attempts.contains(&attempt) {
                sqlx::query("INSERT INTO observation_attempt_projections(attempt_id,projection) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(attempt).bind(value(&Projection::default())?).execute(&mut **tx).await?;
            }
            let projection: Value = sqlx::query_scalar(
            "SELECT projection FROM observation_attempt_projections WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(attempt)
        .fetch_one(&mut **tx)
        .await?;
            attempt_projections.insert(attempt, parse::<Projection>(projection)?);
        }
        Ok(Self {
            current_attempts,
            updated_attempts,
            by_lineage: projections,
            by_attempt: attempt_projections,
        })
    }

    pub fn projection_count(&self) -> usize {
        self.by_lineage.len() + self.by_attempt.len()
    }

    pub fn current_attempts(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.current_attempts.iter().copied()
    }

    pub async fn persist(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        recorded_at: i64,
        events: &[&ObservationRecord],
    ) -> Result<()> {
        let progressed: BTreeSet<_> = events
            .iter()
            .filter(|e| aq_stall_ladder::evidence::is_progress(&e.body.kind))
            .map(|e| e.body.binding.attempt_id)
            .collect();
        for (lineage, projection) in &self.by_lineage {
            projection.validate().map_err(|_| Error::Invalid)?;
            sqlx::query("UPDATE observation_projections SET revision=revision+1,projection=$2,last_recorded_at_ms=GREATEST(last_recorded_at_ms,$3) WHERE lineage_id=$1").bind(lineage).bind(value(projection)?).bind(recorded_at).execute(&mut **tx).await?;
        }
        for (attempt, projection) in &self.by_attempt {
            if !self.updated_attempts.contains(attempt) {
                continue;
            }
            projection.validate().map_err(|_| Error::Invalid)?;
            let at = |seen: bool| if seen { recorded_at } else { 0 };
            sqlx::query("UPDATE observation_attempt_projections SET revision=revision+1,projection=$2,last_recorded_at_ms=GREATEST(last_recorded_at_ms,$3),last_observed_at_ms=GREATEST(last_observed_at_ms,$4),last_progress_at_ms=GREATEST(last_progress_at_ms,$5) WHERE attempt_id=$1").bind(attempt).bind(value(projection)?).bind(recorded_at).bind(at(self.current_attempts.contains(attempt))).bind(at(progressed.contains(attempt))).execute(&mut **tx).await?;
        }
        Ok(())
    }
}

async fn lock_call_owners(
    tx: &mut Transaction<'_, Postgres>,
    keys: &BTreeSet<String>,
    current_attempts: &BTreeSet<Uuid>,
) -> Result<BTreeSet<Uuid>> {
    let mut affected_attempts = current_attempts.clone();
    for key in keys {
        sqlx::query(
            "INSERT INTO observation_call_heads(call_key) VALUES($1) ON CONFLICT DO NOTHING",
        )
        .bind(key)
        .execute(&mut **tx)
        .await?;
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_attempt_id FROM observation_call_heads WHERE call_key=$1 FOR UPDATE",
        )
        .bind(key)
        .fetch_one(&mut **tx)
        .await?;
        if let Some(owner) = owner {
            affected_attempts.insert(owner);
        }
    }
    Ok(affected_attempts)
}
