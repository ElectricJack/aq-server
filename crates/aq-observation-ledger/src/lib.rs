//! Account for an already-admitted batch inside the caller's PostgreSQL transaction.
//! The caller owns authentication, sequence validation, quota admission and commit.
use aq_accounting::{
    pricing::RateCardBook,
    projection::Projection,
    reconcile::{self, Context, Decision, Fact, UnresolvedReason},
    source::OwnerSource,
    wire::*,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod batch_projections;
mod event_accounting;
pub use batch_projections::BatchProjections;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid observation record")]
    Invalid,
    #[error("observation accounting overflow")]
    Capacity,
    #[error("observation database failed")]
    Database(#[from] sqlx::Error),
}
type Result<T> = std::result::Result<T, Error>;

/// Enrollment and revocation status by source ID and generation.
pub type BatchSources = BTreeMap<(Uuid, u64), (OwnerSource, bool)>;

pub struct BatchInput {
    pub sources: BatchSources,
    pub book: RateCardBook,
    pub node: Uuid,
    pub recorded_at: i64,
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| Error::Invalid)
}
fn value<T: Serialize>(input: &T) -> Result<Value> {
    serde_json::to_value(input).map_err(|_| Error::Invalid)
}
fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid)
}

/// Lock projections in stable order, then apply the batch's facts and accounting decisions.
/// Returns the projections to flush and the bytes charged to the node.
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    events: &[&ObservationRecord],
    input: BatchInput,
) -> Result<(BatchProjections, i64)> {
    let keys = usage_keys(events, &input.sources)?;
    let projections = BatchProjections::lock(tx, events, &keys).await?;
    let mut accounting = event_accounting::EventAccounting {
        book: input.book,
        sources: input.sources,
        projections,
        retained: 0,
        node: input.node,
        recorded_at: input.recorded_at,
    };
    let retained = accounting.record(tx, events).await?;
    Ok((accounting.projections, retained))
}

fn usage_keys(events: &[&ObservationRecord], sources: &BatchSources) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for &event in events {
        let p = &event.body.provenance;
        let (source, _) = &sources[&(p.source_id, p.source_generation)];
        if event.body.kind.is_usage() {
            keys.insert(
                reconcile::key(event, &source.policy)
                    .map_err(|_| Error::Invalid)?
                    .as_str()
                    .to_owned(),
            );
        }
    }
    Ok(keys)
}
