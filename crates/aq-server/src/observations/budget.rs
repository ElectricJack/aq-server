//! Lineage budgets from the observation ledger. Retries and wakes share a task
//! lineage, so spend accumulates across attempts and is never reset; replayed
//! calls deduplicate by call key, so they are never recounted. Soft ceilings
//! record policy events. Hard enforcement is out of scope: no hard budget
//! provider is reported, so selection keeps refusing hard requirements.
use super::*;
use aq_accounting::pricing::{self, Summary};

/// Upper bound on canonical calls summarized for one read.
const MAX_HEADS: i64 = 65536;

pub(crate) enum Scope {
    Attempt(Uuid),
    Lineage(Uuid),
    /// Every attempt-owned call the node reported, across all lineages.
    Node(Uuid),
}

/// Canonical call heads in scope, each with the billing basis of the trusted
/// source that recorded its fact.
pub(crate) async fn priced_facts(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope,
) -> Result<Vec<(Option<EstimateBasis>, Fact)>> {
    let (filter, id) = match scope {
        Scope::Attempt(id) => ("h.owner_attempt_id=$1", id),
        // Indexed lineage column, set with the head's owner and immutable.
        Scope::Lineage(id) => ("h.lineage_id=$1", id),
        Scope::Node(id) => ("h.owner_attempt_id IS NOT NULL AND e.node_id=$1", id),
    };
    let rows = sqlx::query(&format!(
        "SELECT h.fact, s.authority->'billing' AS billing FROM observation_call_heads h \
         JOIN observation_events e ON e.event_id=(h.fact#>>'{{event,event_id}}')::uuid \
         LEFT JOIN observation_sources s ON s.node_id=e.node_id \
          AND s.source_id=(e.record#>>'{{body,provenance,source_id}}')::uuid \
          AND s.generation=(e.record#>>'{{body,provenance,source_generation}}')::bigint \
         WHERE {filter} ORDER BY h.call_key LIMIT $2"
    ))
    .bind(id)
    .bind(MAX_HEADS + 1)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() as i64 > MAX_HEADS {
        return Err(Error::Capacity);
    }
    rows.into_iter()
        .map(|row| {
            let billing: Option<Value> = row.get("billing");
            let billing = match billing {
                None | Some(Value::Null) => None,
                Some(v) => Some(parse::<EstimateBasis>(v)?),
            };
            Ok((billing, parse::<Fact>(row.get("fact"))?))
        })
        .collect()
}

pub(crate) fn summarize(
    state: &AppState,
    facts: &[(Option<EstimateBasis>, Fact)],
) -> Result<Summary> {
    summarize_with(&catalog::rate_book(state), facts)
}
fn summarize_with(
    book: &pricing::RateCardBook,
    facts: &[(Option<EstimateBasis>, Fact)],
) -> Result<Summary> {
    let refs: Vec<_> = facts.iter().map(|(b, f)| (b.clone(), f)).collect();
    pricing::summarize(&refs, book).map_err(|_| Error::Invalid)
}

/// Spend accounted for every attempt a node ran (C4 node spend ceilings).
/// An empty summary (no cost components) means the node reported no calls.
pub async fn node(
    pool: &sqlx::PgPool,
    book: &pricing::RateCardBook,
    node: Uuid,
) -> Result<Summary> {
    let mut tx = pool.begin().await?;
    let facts = priced_facts(&mut tx, Scope::Node(node)).await?;
    tx.commit().await?;
    summarize_with(book, &facts)
}

/// A project cost limit without hard enforcement: crossing it is recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftCeiling {
    pub limit_microunits: u64,
    pub currency: String,
}

#[derive(Clone, Debug)]
pub struct LineageBudget {
    pub lineage: Uuid,
    /// Ledger projection revision; grows with every accepted lineage fact.
    pub ledger_revision: u64,
    /// Known lower-bound spend; `None` when costs span currencies.
    pub spent_microunits: Option<u64>,
    pub currency: Option<String>,
    pub accounting: aq_selection::AccountingCompleteness,
    pub soft_ceiling_reached: bool,
}

/// The accounting part of a selection `BudgetSnapshot`.
#[derive(Clone, Debug)]
pub struct AccountingSnapshot {
    pub spent_microunits: Option<u64>,
    pub currency: Option<String>,
    pub accounting: aq_selection::AccountingCompleteness,
    pub accounting_source: aq_selection::ArtifactRef,
    /// Always `None`: the ledger is not a hard budget provider (0004 §2.2).
    pub hard_budget_provider: Option<aq_selection::ArtifactRef>,
}

impl LineageBudget {
    pub fn snapshot(&self) -> AccountingSnapshot {
        AccountingSnapshot {
            spent_microunits: self.spent_microunits,
            currency: self.currency.clone(),
            accounting: self.accounting,
            accounting_source: aq_selection::ArtifactRef {
                id: Text::new("aq.accounting.ledger").expect("constant"),
                version: Text::new(self.ledger_revision.to_string()).expect("decimal"),
                digest: Digest::of(
                    format!(
                        "aq.accounting.ledger/{}/{}",
                        self.lineage, self.ledger_revision
                    )
                    .as_bytes(),
                ),
            },
            hard_budget_provider: None,
        }
    }
}

/// `None` when the lineage has no ledger projection yet.
pub async fn lineage(
    state: &AppState,
    lineage: Uuid,
    soft: Option<&SoftCeiling>,
) -> Result<Option<LineageBudget>> {
    let mut tx = state.store.observation_pool.begin().await?;
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM observation_projections WHERE lineage_id=$1")
            .bind(lineage)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(revision) = revision else {
        tx.commit().await?;
        return Ok(None);
    };
    let ledger_revision = u64::try_from(revision).map_err(|_| Error::Invalid)?;
    let facts = priced_facts(&mut tx, Scope::Lineage(lineage)).await?;
    let summary = summarize(state, &facts)?;
    let currency = summary.currency.as_ref().map(|c| c.as_str().to_owned());
    let soft_ceiling_reached = soft.is_some_and(|c| {
        currency.as_deref() == Some(c.currency.as_str())
            && summary
                .spent_microunits
                .is_some_and(|spent| spent >= c.limit_microunits)
    });
    if let (true, Some(c), Some(spent)) = (soft_ceiling_reached, soft, summary.spent_microunits) {
        let recorded_at = (state.clock)().timestamp_millis();
        sqlx::query(
            "INSERT INTO accounting_policy_events(lineage_id,ceiling,threshold_microunits,spent_microunits,currency,ledger_revision,recorded_at_ms) \
             VALUES($1,'project_soft_cost',$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING",
        )
        .bind(lineage)
        .bind(integer(c.limit_microunits)?)
        .bind(integer(spent)?)
        .bind(&c.currency)
        .bind(revision)
        .bind(recorded_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(LineageBudget {
        lineage,
        ledger_revision,
        spent_microunits: summary.spent_microunits,
        currency,
        accounting: if summary.complete {
            aq_selection::AccountingCompleteness::Complete
        } else {
            aq_selection::AccountingCompleteness::Incomplete
        },
        soft_ceiling_reached,
    }))
}
