//! Reconcile each admitted record against locked projections and persist its evidence.
use super::*;
use aq_accounting::pricing::RateCardBook;

pub(super) struct EventAccounting {
    pub book: RateCardBook,
    pub sources: BatchSources,
    pub projections: BatchProjections,
    pub node: Uuid,
    pub recorded_at: i64,
    pub retained: i64,
}

struct CallHistory {
    key: Digest,
    current: Option<Fact>,
    owner: Option<Uuid>,
    target: Option<Fact>,
}

impl CallHistory {
    async fn load(tx: &mut Transaction<'_, Postgres>, key: Digest, usage: &Usage) -> Result<Self> {
        let (current, owner): (Option<Value>, Option<Uuid>) = sqlx::query_as(
            "SELECT fact,owner_attempt_id FROM observation_call_heads WHERE call_key=$1",
        )
        .bind(key.as_str())
        .fetch_one(&mut **tx)
        .await?;
        let current = current.map(parse::<Fact>).transpose()?;
        let target = if let Some(reference) = &usage.correction_of {
            let v: Option<Value> = sqlx::query_scalar(
                "SELECT fact FROM observation_facts WHERE event_id=$1 AND call_key=$2",
            )
            .bind(reference.event_id)
            .bind(key.as_str())
            .fetch_optional(&mut **tx)
            .await?;
            v.map(parse::<Fact>).transpose()?
        } else {
            None
        };
        Ok(Self {
            key,
            current,
            owner,
            target,
        })
    }

    async fn set_head(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        owner: Uuid,
        fact: &Fact,
    ) -> Result<()> {
        sqlx::query("UPDATE observation_call_heads SET fact=$2,owner_attempt_id=$3,lineage_id=$4 WHERE call_key=$1")
            .bind(self.key.as_str())
            .bind(value(fact)?)
            .bind(owner)
            .bind(fact.lineage)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    fn decide(&self, input: UsageEvent<'_>, book: &RateCardBook) -> Result<Decision> {
        let UsageEvent {
            event,
            usage,
            source,
            revoked,
        } = input;
        // Only token-rate sources pin a rate card. Flat subscription and
        // local usage is recorded but never priced at token rates.
        let pin = match source.billing {
            Some(EstimateBasis::TokenRates) => book
                .current(&usage.call.provider, &usage.call.model)
                .map(|card| card.reference.clone()),
            _ => None,
        };
        // Late usage from a superseded generation is kept as evidence but
        // never charged under its revoked policy, and leaves the lineage
        // accounting incomplete rather than stalling the node sequence.
        let decision = if revoked || usage.call.account_reference != source.account_claim {
            Decision::Unresolved {
                reason: UnresolvedReason::AttributionChanged,
            }
        } else {
            match reconcile::decide(
                event,
                Context {
                    lineage: source.binding.lineage_id,
                    policy: &source.policy,
                    current: self.current.as_ref(),
                    correction_target: self.target.as_ref(),
                    baseline: None,
                    pricing_reference: pin.as_ref(),
                },
            ) {
                Ok(v) => v,
                Err(aq_accounting::Error::Conflict | aq_accounting::Error::Unsupported) => {
                    Decision::Unresolved {
                        reason: UnresolvedReason::AttributionChanged,
                    }
                }
                Err(_) => return Err(Error::Invalid),
            }
        };
        Ok(decision)
    }
}

#[derive(Clone, Copy)]
struct UsageEvent<'a> {
    event: &'a ObservationRecord,
    usage: &'a Usage,
    source: &'a OwnerSource,
    revoked: bool,
}

struct RecordedDecision {
    value: Value,
    appended: Option<(Digest, Box<Fact>)>,
}

impl EventAccounting {
    pub async fn record(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        events: &[&ObservationRecord],
    ) -> Result<i64> {
        for &event in events {
            let result = self.decision(tx, event).await?;
            self.store_event(tx, event, result).await?;
        }
        Ok(self.retained)
    }

    async fn decision(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        event: &ObservationRecord,
    ) -> Result<RecordedDecision> {
        let provenance = &event.body.provenance;
        let (source, revoked) =
            &self.sources[&(provenance.source_id, provenance.source_generation)];
        match &event.body.kind {
            ObservationKind::Usage { usage } => {
                let key = reconcile::key(event, &source.policy).map_err(|_| Error::Invalid)?;
                let history = CallHistory::load(tx, key, usage).await?;
                let input = UsageEvent {
                    event,
                    usage,
                    source,
                    revoked: *revoked,
                };
                let decision = history.decide(input, &self.book)?;
                let appended = match &decision {
                    Decision::Append { fact, .. } => {
                        let owner = self.update_projections(&event.body.binding, &history, fact)?;
                        history.set_head(tx, owner, fact).await?;
                        Some((history.key, fact.clone()))
                    }
                    Decision::Unresolved { .. } => {
                        self.mark_unresolved(&event.body.binding)?;
                        None
                    }
                    Decision::NoCharge { .. } => None,
                };
                Ok(RecordedDecision {
                    value: value(&decision)?,
                    appended,
                })
            }
            kind => {
                if matches!(kind, ObservationKind::Gap { .. }) {
                    self.mark_unresolved(&event.body.binding)?;
                }
                Ok(RecordedDecision {
                    value: json!({"kind":"observation_only"}),
                    appended: None,
                })
            }
        }
    }

    fn mark_unresolved(&mut self, binding: &Binding) -> Result<()> {
        for entry in [
            self.projections.by_lineage.get_mut(&binding.lineage_id),
            self.projections.by_attempt.get_mut(&binding.attempt_id),
        ] {
            let projection = entry.ok_or(Error::Invalid)?;
            projection.unresolved_events = projection
                .unresolved_events
                .checked_add(1)
                .ok_or(Error::Capacity)?;
        }
        Ok(())
    }

    fn update_projections(
        &mut self,
        binding: &Binding,
        history: &CallHistory,
        fact: &Fact,
    ) -> Result<Uuid> {
        let projection = self
            .projections
            .by_lineage
            .get_mut(&binding.lineage_id)
            .ok_or(Error::Invalid)?;
        *projection = projection
            .replace_fact(history.current.as_ref(), fact)
            .map_err(|_| Error::Capacity)?;
        let owner = history.owner.unwrap_or(binding.attempt_id);
        self.projections.updated_attempts.insert(owner);
        let owned = self
            .projections
            .by_attempt
            .get_mut(&owner)
            .ok_or(Error::Invalid)?;
        *owned = owned
            .replace_fact(history.current.as_ref(), fact)
            .map_err(|_| Error::Capacity)?;
        Ok(owner)
    }

    async fn store_event(
        &mut self,
        tx: &mut Transaction<'_, Postgres>,
        event: &ObservationRecord,
        result: RecordedDecision,
    ) -> Result<()> {
        let RecordedDecision {
            value: decision,
            appended,
        } = result;
        self.retained = self
            .retained
            .checked_add(serde_json::to_vec(event).map_err(|_| Error::Invalid)?.len() as i64)
            .and_then(|v| v.checked_add(decision.to_string().len() as i64))
            .ok_or(Error::Capacity)?;
        sqlx::query("INSERT INTO observation_events(event_id,node_id,sequence,lineage_id,attempt_id,record,decision,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(event.event_id).bind(self.node).bind(integer(event.body.sequence)?).bind(event.body.binding.lineage_id).bind(event.body.binding.attempt_id).bind(value(event)?).bind(decision).bind(self.recorded_at).execute(&mut **tx).await?;
        if let Some((key, fact)) = appended {
            self.retained = self
                .retained
                .checked_add(
                    (serde_json::to_vec(&fact).map_err(|_| Error::Invalid)?.len() * 2) as i64,
                )
                .ok_or(Error::Capacity)?;
            sqlx::query("INSERT INTO observation_facts(event_id,call_key,fact) VALUES($1,$2,$3)")
                .bind(event.event_id)
                .bind(key.as_str())
                .bind(value(&fact)?)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
}
