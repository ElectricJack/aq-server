use super::*;

pub async fn owner_availability(conn: &mut PgConnection) -> Result<OwnerAvailability, Error> {
    let row = sqlx::query(
        "SELECT o.available, o.generation, o.operation_id, COALESCE(u.role = 'owner', FALSE) AS by_owner, \
         (extract(epoch FROM o.updated_at) * 1000)::bigint AS at_ms \
         FROM owner_availability o LEFT JOIN users u ON u.id = o.updated_by",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Ok(OwnerAvailability {
            state: Availability::Unset,
            generation: 1,
            updated_at_ms: None,
            operation_id: None,
            set_by_former_owner: false,
        });
    };
    let by_owner: bool = r.try_get("by_owner")?;
    Ok(OwnerAvailability {
        state: match (by_owner, r.try_get("available")?) {
            (false, _) => Availability::Unset,
            (true, true) => Availability::On,
            (true, false) => Availability::Off,
        },
        generation: ms(r.try_get("generation")?),
        updated_at_ms: Some(ms(r.try_get("at_ms")?)),
        operation_id: Some(r.try_get("operation_id")?),
        set_by_former_owner: !by_owner,
    })
}

/// Spec 0007 Q2: the owner's attestation of `credentials` at `now_ms` while the
/// switch is on (`aq_intelligence_config::owner_attestations`), else none.
/// Its generation is the only fact a fence compares, so a flip refuses.
pub async fn owner_attested(
    conn: &mut PgConnection,
    credentials: &[aq_selection::AuthorizedCredential],
    observed: &[aq_selection::AvailabilityObservation],
    at: AttestationTime,
) -> Result<Vec<aq_selection::AvailabilityObservation>, Error> {
    Ok(match owner_switch(conn).await? {
        Some(on) => aq_intelligence_config::owner_attestations(credentials, observed, on, at),
        None => vec![],
    })
}

/// Spec 0007 Q2: `(principal, generation)` of the owner's switch while it is
/// on; None when unset or off, below 101, or (fix round 2) when the user who
/// switched it on is no longer an owner: that switch counts as unset.
/// Selection synthesizes the owner's attestation from it, so the admission
/// fences change only when the owner flips the switch.
pub async fn owner_switch(conn: &mut PgConnection) -> Result<Option<(Uuid, u64)>, Error> {
    if !has_format(conn).await? {
        return Ok(None);
    }
    let on: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT o.updated_by, o.generation FROM owner_availability o \
         JOIN users u ON u.id = o.updated_by AND u.role = 'owner' WHERE o.available FOR SHARE OF o",
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(on.map(|(by, g)| (by, ms(g))))
}

/// Compare-and-swap on the singleton, as `set_desired`. Setting the state it
/// already has (on to on, off to off) at the current generation changes
/// nothing and returns the current generation (fix round 2).
pub async fn set_owner_availability(
    tx: &mut PgConnection,
    available: bool,
    change: OwnerChange,
) -> Result<OwnerAvailability, Error> {
    let OwnerChange {
        expected_generation,
        actor,
        operation_id,
        now,
    } = change;
    let change = AvailabilityChange {
        available,
        expected_generation,
        actor,
        operation_id,
        now,
    };
    // N2: a no-op or a stale generation locks and writes no node row.
    let current = owner_availability(tx).await?;
    if change.unchanged(&current) {
        return Ok(current);
    }
    if current.generation != expected_generation {
        let generation = current.generation;
        return Err(Error::Conflict { generation });
    }
    // Fix round 3 (I2 race): touch the live node rows first, as set_desired
    // does, so a publication racing this change waits or fails to serialize.
    // N1: locked in id order, with a bounded wait. The state is re-read under them.
    bound(tx).await?;
    lock_live_nodes(tx, None).await?;
    sqlx::query("UPDATE nodes SET machine_key=machine_key WHERE revoked_at IS NULL")
        .execute(&mut *tx)
        .await?;
    let current = owner_availability(tx).await?;
    if change.unchanged(&current) {
        return Ok(current);
    }
    change.write(tx).await?;
    owner_availability(tx).await
}

struct AvailabilityChange {
    available: bool,
    expected_generation: u64,
    actor: Uuid,
    operation_id: Uuid,
    now: DateTime<Utc>,
}
impl AvailabilityChange {
    // A former owner's switch is unset, so setting it again records the current owner.
    fn unchanged(&self, current: &OwnerAvailability) -> bool {
        current.generation == self.expected_generation
            && matches!(
                (current.state, self.available),
                (Availability::On, true) | (Availability::Off, false)
            )
    }
    async fn write(&self, tx: &mut PgConnection) -> Result<(), Error> {
        let done = if self.expected_generation == 1 {
            sqlx::query(
            "INSERT INTO owner_availability(available,generation,operation_id,updated_at,updated_by) \
             VALUES($1,2,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(self.available)
        .bind(self.operation_id)
        .bind(self.now)
        .bind(self.actor)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        } else {
            0
        };
        if done == 0 {
            let updated = sqlx::query(
            "UPDATE owner_availability SET available=$1, generation=$2, operation_id=$3, updated_at=$4, updated_by=$5 \
             WHERE generation=$6",
        )
        .bind(self.available)
        .bind(self.expected_generation as i64 + 1)
        .bind(self.operation_id)
        .bind(self.now)
        .bind(self.actor)
        .bind(self.expected_generation as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
            if updated == 0 {
                let current = owner_availability(tx).await?;
                return Err(Error::Conflict {
                    generation: current.generation,
                });
            }
        }
        Ok(())
    }
}
