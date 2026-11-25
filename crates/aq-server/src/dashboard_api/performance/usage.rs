use crate::{AppState, execution::authority, observations::catalog};
use aq_accounting::{
    Category,
    pricing::{self, RateCardBook},
    projection::Projection,
    reconcile::Fact,
    wire::{Cost, EstimateBasis},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_CALLS: usize = 8_192;
const MAX_ATTEMPTS: usize = 2_048;
const MAX_TASK_ATTEMPTS: usize = 64;
const MAX_MODELS: usize = 64;

type Costs = BTreeMap<(String, &'static str), u64>;
type Models = BTreeMap<(String, Option<String>), Totals>;

#[derive(Default)]
struct Totals {
    tokens: Projection,
    priced: u64,
    costs: Costs,
}

fn cost_component(cost: &Cost) -> Option<(String, &'static str, u64)> {
    match cost {
        Cost::ProviderReported {
            currency,
            amount_microunits,
            ..
        } => Some((
            currency.as_str().to_owned(),
            "provider_reported",
            *amount_microunits,
        )),
        Cost::NonInvoiceEstimate {
            currency,
            basis,
            amount_microunits,
            ..
        } => {
            let name = match basis {
                EstimateBasis::TokenRates => "token_rates",
                EstimateBasis::SubscriptionAllocation => "subscription_allocation",
                EstimateBasis::LocalModelAllocation => "local_model_allocation",
            };
            Some((currency.as_str().to_owned(), name, *amount_microunits))
        }
        Cost::Unknown { .. } => None,
    }
}

fn cost_rows(costs: &Costs) -> Vec<Value> {
    costs.iter().map(|((currency, basis), amount)| {
        json!({"currency": currency, "basis": basis, "microunits": amount})
    }).collect()
}

impl Totals {
    fn include(
        &mut self,
        fact: &Fact,
        cost: &Option<(String, &'static str, u64)>,
    ) -> anyhow::Result<()> {
        self.tokens = self.tokens.replace_fact(None, fact)?;
        if let Some((currency, basis, amount)) = cost {
            self.priced += 1;
            let total = self.costs.entry((currency.clone(), basis)).or_default();
            *total = total
                .checked_add(*amount)
                .ok_or_else(|| anyhow::anyhow!("usage cost overflow"))?;
        }
        Ok(())
    }
}

fn model_rows(models: &Models) -> Vec<Value> {
    models
        .iter()
        .take(MAX_MODELS)
        .map(|((model, harness), group)| {
            let tokens = &group.tokens;
            let counts = Category::ALL.map(|category| {
                let i = category as usize;
                (tokens.unknown_contributions[i] < tokens.canonical_calls)
                    .then_some(tokens.known_subtotals[i])
            });
            json!({
                "model": model, "harness": harness, "calls": tokens.canonical_calls,
                "missing_prices": tokens.canonical_calls - group.priced,
                "incomplete_calls": tokens.incomplete_calls,
                "tokens": {
                    "input": counts[0], "output": counts[1], "cache_read": counts[2],
                    "cache_write": counts[3], "reasoning": counts[4],
                },
                "costs": cost_rows(&group.costs),
            })
        })
        .collect()
}

#[derive(Default)]
struct Attempt {
    totals: Totals,
    models: Models,
    unresolved: u64,
    expected: bool,
    intended_model: Option<String>,
    state: Option<String>,
}

impl Attempt {
    fn complete(&self) -> bool {
        let mut tokens = self.totals.tokens.clone();
        tokens.unresolved_events = self.unresolved;
        tokens.all_categories_represented().unwrap_or(false)
    }
}

struct Report {
    totals: Totals,
    unassigned: Totals,
    models: Models,
    attempts: BTreeMap<Uuid, Attempt>,
    truncated: bool,
    cutoff: i64,
}

impl Report {
    fn view(&self) -> Value {
        let expected = self.attempts.values().filter(|a| a.expected).count();
        let complete = self
            .attempts
            .values()
            .filter(|a| a.expected && a.complete())
            .count();
        let unresolved: u64 = self.attempts.values().map(|a| a.unresolved).sum();
        json!({
            "status": status(&self.totals, complete < expected || self.truncated || self.models.len() > MAX_MODELS),
            "models": model_rows(&self.models),
            "costs": cost_rows(&self.totals.costs),
            "unassigned": {
                "canonical_calls": self.unassigned.tokens.canonical_calls,
                "priced_calls": self.unassigned.priced,
                "costs": cost_rows(&self.unassigned.costs),
            },
            "truncated": self.truncated || self.models.len() > MAX_MODELS,
            "window_basis": "first_canonical_receipt",
            "window_start_ms": self.cutoff.saturating_sub(WEEK_MS),
            "window_cutoff_ms": self.cutoff,
            "coverage": {
                "expected_attempts": expected, "complete_attempts": complete,
                "canonical_calls": self.totals.tokens.canonical_calls,
                "priced_calls": self.totals.priced, "unresolved_events": unresolved,
            },
        })
    }
}

fn status(totals: &Totals, partial: bool) -> &'static str {
    if partial || totals.tokens.canonical_calls > totals.priced {
        "partial"
    } else if totals.tokens.canonical_calls == 0 {
        "no_events"
    } else {
        "observed"
    }
}

async fn load(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    book: &RateCardBook,
    project: Option<Uuid>,
    task: Option<Uuid>,
    cutoff: i64,
) -> anyhow::Result<Report> {
    let start = cutoff.saturating_sub(WEEK_MS);
    let max_attempts = if task.is_some() {
        MAX_TASK_ATTEMPTS
    } else {
        MAX_ATTEMPTS
    };
    let attempts = sqlx::query(include_str!("usage_attempts.sql"))
        .bind(project)
        .bind(task)
        .bind(start)
        .bind(cutoff)
        .bind((max_attempts + 1) as i64)
        .fetch_all(&mut **tx)
        .await?;
    let calls = sqlx::query(include_str!("usage_calls.sql"))
        .bind(project)
        .bind(task)
        .bind(start)
        .bind(cutoff)
        .bind((MAX_CALLS + 1) as i64)
        .fetch_all(&mut **tx)
        .await?;
    let mut report = Report {
        totals: Totals::default(),
        unassigned: Totals::default(),
        models: Models::new(),
        attempts: BTreeMap::new(),
        truncated: calls.len() > MAX_CALLS || attempts.len() > max_attempts,
        cutoff,
    };
    for row in attempts.into_iter().take(max_attempts) {
        report.attempts.insert(
            row.try_get("attempt_id")?,
            Attempt {
                unresolved: row.try_get::<i64, _>("unresolved")?.try_into()?,
                expected: row.try_get("expected")?,
                intended_model: row.try_get("intended_model")?,
                state: row.try_get("attempt_status")?,
                ..Attempt::default()
            },
        );
    }
    for row in calls.into_iter().take(MAX_CALLS) {
        let fact: Fact = serde_json::from_value(row.try_get("fact")?)?;
        let billing: Option<EstimateBasis> = serde_json::from_value(
            row.try_get::<Option<Value>, _>("billing")?
                .unwrap_or(Value::Null),
        )?;
        let priced = pricing::summarize(&[(billing, &fact)], book)?;
        let cost = priced.costs.iter().find_map(cost_component);
        let key = (
            fact.usage.call.model.as_str().to_owned(),
            row.try_get("harness")?,
        );
        report.totals.include(&fact, &cost)?;
        if row.try_get("unassigned")? {
            report.unassigned.include(&fact, &cost)?;
        }
        report
            .models
            .entry(key.clone())
            .or_default()
            .include(&fact, &cost)?;
        let attempt_id = row.try_get("owner_attempt_id")?;
        if let Some(attempt) = report.attempts.get_mut(&attempt_id) {
            attempt.totals.include(&fact, &cost)?;
            if task.is_some() {
                attempt
                    .models
                    .entry(key)
                    .or_default()
                    .include(&fact, &cost)?;
            }
        } else {
            report.truncated = true;
        }
    }
    Ok(report)
}

pub(super) async fn summary(
    state: &AppState,
    project: Option<Uuid>,
    cutoff: i64,
) -> anyhow::Result<Value> {
    let book = catalog::rate_book(state);
    let mut tx = authority::snapshot(&state.store.observation_pool).await?;
    let report = load(&mut tx, &book, project, None, cutoff).await?;
    let mut view = report.view();
    if project.is_none() {
        let overhead: Value = sqlx::query_scalar(
            "SELECT jsonb_build_object(
                'scope', 'finished_internal_runs',
                'finished_runs', count(*),
                'reported_runs', count(usage),
                'input_tokens', sum((usage->>'input_tokens')::numeric),
                'output_tokens', sum((usage->>'output_tokens')::numeric)
             ) FROM internal_tasks
             WHERE status IN ('answered','failed') AND affected_kind<>'task'
               AND finished_at_ms BETWEEN $1 AND $2",
        )
        .bind(cutoff.saturating_sub(WEEK_MS))
        .bind(cutoff)
        .fetch_one(&mut *tx)
        .await?;
        view["shared_overhead"] = overhead;
    }
    tx.commit().await?;
    Ok(view)
}

pub(super) async fn task(
    state: &AppState,
    project: Option<Uuid>,
    task_id: Uuid,
    cutoff: i64,
) -> anyhow::Result<Value> {
    let book = catalog::rate_book(state);
    let mut tx = authority::snapshot(&state.store.observation_pool).await?;
    let report = load(&mut tx, &book, project, Some(task_id), cutoff).await?;
    let first_route: Option<Value> = sqlx::query_scalar(include_str!("usage_route.sql"))
        .bind(task_id)
        .bind(project)
        .bind(cutoff)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if report.attempts.is_empty() && first_route.is_none() {
        return Ok(Value::Null);
    }
    let attempts: Vec<_> = report.attempts.iter().map(|(id, attempt)| json!({
        "attempt_id": id, "status": status(&attempt.totals, (attempt.expected && !attempt.complete()) || attempt.models.len() > MAX_MODELS),
        "attempt_status": attempt.state, "intended_model": attempt.intended_model,
        "models": model_rows(&attempt.models), "costs": cost_rows(&attempt.totals.costs),
        "unresolved_events": attempt.unresolved,
    })).collect();
    let mut view = report.view();
    view["first_route"] = json!(first_route);
    view["attempts"] = json!(attempts);
    Ok(view)
}
