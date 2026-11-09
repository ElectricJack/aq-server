//! Bounded owner observations. None of these projections grants control or
//! promotes process results / settlement evaluations into Graph completion.
use super::{
    contract::*,
    runtime::Runtime,
    service::{auth, envelope, fence, stamp, timestamp, tx},
    *,
};
use crate::{
    execution::{
        authority::{self, Policy},
        profiled::{Retention, Status},
    },
    profiled_execution::{human_results, settlement},
    store::User,
};
use serde_json::Value;
use sqlx::Row;
use std::{collections::BTreeMap, future::Future};

const PROJECTS: usize = 8;
const MEMBERS: usize = 100;
const SOURCE_BUDGET: Duration = Duration::from_millis(650);

#[derive(Clone)]
struct Project {
    label: ProjectRef,
    revision: i64,
    binding: Value,
}
#[derive(Default)]
struct Contribution {
    sources: Vec<SourceObservation>,
    attention: Vec<AttentionItem>,
    outcomes: Vec<RecentOutcome>,
    running: Option<Total>,
    recovering: Option<Total>,
    ready: Option<Total>,
    needs_you: Option<Total>,
    per_project_running: BTreeMap<Uuid, u32>,
}
fn total(value: usize, truncated: bool) -> Total {
    Total {
        value: Some(value as u32),
        accuracy: if truncated {
            Accuracy::Estimated
        } else {
            Accuracy::Exact
        },
    }
}
fn source(
    name: &str,
    project: Option<ProjectRef>,
    at: u64,
    count: usize,
    truncated: bool,
    reason: Option<&str>,
) -> Result<SourceObservation> {
    let partial = truncated || reason.is_some();
    Ok(SourceObservation {
        source: name.into(),
        project,
        observation: Some(Observation {
            observed_at: timestamp(at)?,
            fresh_until: timestamp(at.saturating_add(5000))?,
            source: name.into(),
            quality: Quality::Reported,
            coverage: if partial {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
        }),
        coverage: Coverage {
            state: if partial {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
            reason: reason
                .map(str::to_owned)
                .or_else(|| truncated.then(|| "bounded_sample".into())),
        },
        total: total(count, truncated),
    })
}
fn unavailable(name: &str, reason: &str) -> SourceObservation {
    SourceObservation {
        source: name.into(),
        project: None,
        observation: None,
        coverage: Coverage {
            state: Completeness::Unknown,
            reason: Some(reason.into()),
        },
        total: Total::unknown(),
    }
}
async fn bounded_source(
    name: &str,
    future: impl Future<Output = Result<Contribution>>,
) -> Contribution {
    match tokio::time::timeout(SOURCE_BUDGET, Box::pin(future)).await {
        Ok(Ok(c)) => c,
        _ => Contribution {
            sources: vec![unavailable(name, "owner_unavailable")],
            ..Default::default()
        },
    }
}
fn reference(kind: &str, id: Uuid, revision: ReferenceRevision) -> EntityRef {
    EntityRef {
        kind: kind.into(),
        id,
        revision: Some(revision),
    }
}
fn digest(value: &str) -> Result<ReferenceRevision> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Unavailable);
    }
    Ok(ReferenceRevision::Digest(Digest(format!("sha256:{value}"))))
}
fn attention(
    kind: AttentionKind,
    subject: EntityRef,
    project: Option<ProjectRef>,
    title: &str,
    reason: &str,
    since: Option<Timestamp>,
) -> AttentionItem {
    AttentionItem {
        id: format!(
            "{}:{}:{}",
            subject.kind,
            subject.id,
            project
                .as_ref()
                .map(|p| p.id.to_string())
                .unwrap_or_default()
        ),
        kind,
        subject,
        project,
        title: title.into(),
        reason: reason.into(),
        impact: "Review the owning record for current context".into(),
        since,
        action: None,
    }
}

pub(super) async fn overview(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    scope: Scope,
    mounted: MountedOwners,
) -> Result<Envelope<Overview>> {
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        fence(state, runtime, user, &a).await?;
        return envelope(
            state,
            scope,
            Overview {
                counts: Counts {
                    running: Total::unknown(),
                    ready: Total::unknown(),
                    needs_you: Total::unknown(),
                    recovering: Total::unknown(),
                },
                attention: vec![],
                in_motion: vec![],
                recent_outcomes: vec![],
                sources: vec![unavailable("overview", "owner_visibility_required")],
            },
            Some("owner_visibility_required"),
        );
    }
    let mut transaction = tx(runtime).await?;
    let rows = sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id=$1) ORDER BY project_id LIMIT 9")
        .bind(scope.project()).fetch_all(&mut *transaction).await?;
    let truncated = rows.len() > PROJECTS;
    let projects: Vec<Project> = rows
        .into_iter()
        .take(PROJECTS)
        .map(|row| {
            let id = row.get("project_id");
            let binding: Value = row.get("binding");
            Project {
                label: ProjectRef {
                    id,
                    label: binding["name"]
                        .as_str()
                        .filter(|s| s.len() <= 512)
                        .unwrap_or("Project")
                        .into(),
                },
                revision: row.get("revision"),
                binding,
            }
        })
        .collect();
    transaction.commit().await?;
    let ids: Vec<Uuid> = projects.iter().map(|p| p.label.id).collect();
    let labels: BTreeMap<Uuid, ProjectRef> = projects
        .iter()
        .map(|p| (p.label.id, p.label.clone()))
        .collect();
    // SO-01: results holds node row locks for its whole page while attempts
    // locks each node with a 250 ms timeout. Run concurrently, attempts lost
    // that wait on ~40% of reads, so these two sources run in turn.
    let ((results, attempts), ready, reviews, settlements, operations) = tokio::join!(
        Box::pin(async {
            let results = bounded_source(
                "results",
                results(state, user, &projects, truncated, mounted.results),
            )
            .await;
            // Another request (a sessions or terminal read) may still hold one
            // node briefly: retry once within the same source budget.
            let all = scope.project().is_none();
            let attempts = bounded_source("attempts_and_workers", async {
                match attempts(state, runtime, &ids, &labels, truncated, all).await {
                    Err(_) => attempts(state, runtime, &ids, &labels, truncated, all).await,
                    done => done,
                }
            })
            .await;
            (results, attempts)
        }),
        Box::pin(bounded_source(
            "graph_ready",
            ready(state, &projects, truncated, mounted.graph)
        )),
        Box::pin(bounded_source(
            "reviews",
            reviews(state, runtime, user, &projects, truncated)
        )),
        Box::pin(bounded_source(
            "settlement",
            settlements(state, runtime, user, &projects, truncated)
        )),
        Box::pin(bounded_source(
            "operations",
            operations(state, runtime, user, &scope, &labels, truncated)
        )),
    );
    let counts = Counts {
        running: attempts.running.clone().unwrap_or_else(Total::unknown),
        recovering: attempts.recovering.clone().unwrap_or_else(Total::unknown),
        ready: ready.ready.clone().unwrap_or_else(Total::unknown),
        needs_you: reviews.needs_you.clone().unwrap_or_else(Total::unknown),
    };
    let in_motion = projects
        .iter()
        .map(|p| InMotion {
            project: p.label.clone(),
            running: if attempts.running.is_some() {
                total(
                    *attempts.per_project_running.get(&p.label.id).unwrap_or(&0) as usize,
                    truncated || matches!(counts.running.accuracy, Accuracy::Estimated),
                )
            } else {
                Total::unknown()
            },
            next_constraint: None,
            last_progress_at: None,
        })
        .collect();
    let mut data = Overview {
        counts,
        attention: vec![],
        in_motion,
        recent_outcomes: vec![],
        sources: vec![
            unavailable("questions", "question_owner_not_mounted"),
            unavailable("blocked_tasks", "blocked_task_owner_not_mounted"),
        ],
    };
    let decisions = reviews.needs_you.is_some();
    for c in [attempts, ready, reviews, results, settlements, operations] {
        data.sources.extend(c.sources);
        data.attention.extend(c.attention);
        data.recent_outcomes.extend(c.outcomes);
    }
    if truncated {
        data.sources
            .push(unavailable("projects", "project_sample_truncated"));
    }
    // SO-07: "need you" counts the attention list it heads, not only decisions
    // (the decision count stays the `reviews` source total). Unknown while the
    // decision owner is; estimated while any source is partial or unknown.
    if decisions {
        let complete = data
            .sources
            .iter()
            .all(|s| matches!(s.coverage.state, Completeness::Complete));
        data.counts.needs_you = total(data.attention.len(), !complete);
    }
    // Re-read bindings after all owner awaits. A different workspace/name/revision
    // must never be attached to observations made against the original binding.
    let mut transaction = tx(runtime).await?;
    let latest = sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id").bind(&ids).fetch_all(&mut *transaction).await?;
    if latest.len() != projects.len()
        || latest.iter().any(|r| {
            projects
                .iter()
                .find(|p| p.label.id == r.get::<Uuid, _>("project_id"))
                .is_none_or(|p| {
                    p.revision != r.get::<i64, _>("revision")
                        || p.binding != r.get::<Value, _>("binding")
                })
        })
    {
        return Err(Error::AuthorizationChanged);
    }
    transaction.commit().await?;
    fence(state, runtime, user, &a).await?;
    let now = stamp(state)?;
    for s in &data.sources {
        if let Some(o) = &s.observation {
            let at = chrono::DateTime::parse_from_rfc3339(&o.observed_at.0)
                .map_err(|_| Error::Unavailable)?
                .timestamp_millis();
            let until = chrono::DateTime::parse_from_rfc3339(&o.fresh_until.0)
                .map_err(|_| Error::Unavailable)?
                .timestamp_millis();
            if now < at as u64 || now >= until as u64 {
                return Err(Error::Unavailable);
            }
        }
    }
    data.recent_outcomes.sort_by(|a, b| {
        b.at.0
            .cmp(&a.at.0)
            .then_with(|| a.subject.id.cmp(&b.subject.id))
    });
    let pending = data
        .attention
        .iter()
        .filter(|a| matches!(a.kind, AttentionKind::UncertainOperation))
        .map(|a| a.subject.id)
        .collect();
    let mut reply = envelope(
        state,
        scope,
        data,
        Some("See per-source coverage; unavailable owners do not contribute zero"),
    )?;
    reply.pending_operations = pending;
    Ok(reply)
}

async fn ready(
    state: &AppState,
    projects: &[Project],
    truncated: bool,
    mounted: bool,
) -> Result<Contribution> {
    if !mounted {
        return Err(Error::Unavailable);
    }
    let graph = state.agent_access.as_ref().ok_or(Error::Unavailable)?;
    let mut c = Contribution::default();
    let mut n = 0;
    let mut more = truncated;
    for p in projects {
        if p.binding["graph_adapter_reference"] != "aq.graph.configured" {
            return Err(Error::Unavailable);
        }
        let workspace = p.binding["graph_workspace_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or(Error::Unavailable)?;
        let page = graph
            .scheduling_ready_current(workspace, 32, None, state.clock.as_ref())
            .await
            .map_err(|_| Error::Unavailable)?;
        n += page.items.len();
        more |= page.next_cursor.is_some();
        let mut s = source(
            "graph_ready",
            Some(p.label.clone()),
            page.stamp.received_at_ms,
            page.items.len(),
            page.next_cursor.is_some() || truncated,
            None,
        )?;
        if let Some(o) = &mut s.observation {
            o.fresh_until = timestamp(page.stamp.expires_at_ms)?;
        }
        c.sources.push(s);
    }
    if projects.is_empty() {
        c.sources.push(source(
            "graph_ready",
            None,
            stamp(state)?,
            0,
            truncated,
            None,
        )?);
    }
    c.ready = Some(total(n, more));
    Ok(c)
}

const WORKER: &str = "SELECT id,name,worker_version,protocol_version,enrolled_at,last_seen_at,revoked_at,xmin::text AS revision FROM nodes WHERE id=$1 AND revoked_at IS NULL";
const WORKERS: &str = "SELECT id,name,worker_version,protocol_version,enrolled_at,last_seen_at,revoked_at,xmin::text AS revision FROM nodes WHERE revoked_at IS NULL ORDER BY id LIMIT 101";
fn offline(
    state: &AppState,
    w: &sqlx::postgres::PgRow,
    project: Option<ProjectRef>,
) -> Result<Option<AttentionItem>> {
    let seen: Option<chrono::DateTime<chrono::Utc>> = w.get("last_seen_at");
    let worker = crate::store::Node {
        id: w.get("id"),
        name: w.get("name"),
        worker_version: w.get("worker_version"),
        protocol_version: w.get::<i32, _>("protocol_version") as u32,
        enrolled_at: w.get("enrolled_at"),
        last_seen_at: seen,
        revoked_at: w.get("revoked_at"),
    };
    let after = Duration::from_secs(state.config.workers.offline_after_secs);
    if crate::admin_api::worker_status(&worker, (state.clock)(), after) != "offline" {
        return Ok(None);
    }
    let since = seen
        .map(|at| {
            timestamp((at.timestamp_millis() as u64).saturating_add(after.as_millis() as u64))
        })
        .transpose()?;
    Ok(Some(attention(
        AttentionKind::WorkerOffline,
        reference("worker", worker.id, Revision(w.get("revision")).into()),
        project,
        "Worker offline",
        if seen.is_some() {
            "Heartbeat exceeds the configured offline threshold"
        } else {
            "No heartbeat has been observed"
        },
        since,
    )))
}
async fn attempts(
    state: &AppState,
    runtime: &Runtime,
    ids: &[Uuid],
    labels: &BTreeMap<Uuid, ProjectRef>,
    truncated: bool,
    installation: bool,
) -> Result<Contribution> {
    let mut transaction = tx(runtime).await?;
    let rows = sqlx::query("SELECT attempt_id,node_id FROM execution_attempts WHERE (CASE WHEN kind='profiled_v1' THEN state#>>'{offer,manifest,bindings,project_id}' ELSE state#>>'{offer,spec,project_id}' END)=ANY($1) ORDER BY attempt_id LIMIT 101")
        .bind(ids.iter().map(ToString::to_string).collect::<Vec<_>>()).fetch_all(&mut *transaction).await?;
    transaction.commit().await?;
    let more = truncated || rows.len() > MEMBERS;
    let mut c = Contribution::default();
    let mut running = 0;
    let mut recovering = 0;
    let mut unknown_running = false;
    let mut workers = BTreeMap::new();
    for row in rows.iter().take(MEMBERS) {
        let mut transaction = tx(runtime).await?;
        let node = row.get("node_id");
        let attempt = row.get("attempt_id");
        let v =
            authority::read_attempt(state, &mut transaction, node, attempt, Policy::IdentityOnly)
                .await
                .map_err(|_| Error::Unavailable)?;
        if !ids.contains(&v.project_id) {
            return Err(Error::Unavailable);
        }
        if v.running_at(stamp(state)?) {
            running += 1;
            *c.per_project_running.entry(v.project_id).or_insert(0) += 1;
        } else if v.active_at(stamp(state)?).is_ok() {
            unknown_running = true;
        }
        if v.holds_task
            && (matches!(v.status, Status::Cancelling | Status::Unknown)
                || (v.stopped
                    && matches!(v.retention, Some(Retention::Pending | Retention::Unknown))))
        {
            recovering += 1;
        }
        let worker = sqlx::query(WORKER)
            .bind(node)
            .fetch_optional(&mut *transaction)
            .await?;
        if let Some(w) = worker
            && let Some(item) = offline(state, &w, labels.get(&v.project_id).cloned())?
        {
            workers.insert((node, Some(v.project_id)), item);
        }
        transaction.commit().await?;
    }
    c.sources.push(source(
        "attempts",
        None,
        stamp(state)?,
        rows.len().min(MEMBERS),
        more,
        None,
    )?);
    // SO-06: across all projects every enrolled offline worker is attention,
    // not only those linked to retained attempts (those keep their project).
    let mut nodes_more = false;
    if installation {
        let mut transaction = tx(runtime).await?;
        let rows = sqlx::query(WORKERS).fetch_all(&mut *transaction).await?;
        transaction.commit().await?;
        nodes_more = rows.len() > MEMBERS;
        for w in rows.iter().take(MEMBERS) {
            let id: Uuid = w.get("id");
            if !workers.keys().any(|(node, _)| *node == id)
                && let Some(item) = offline(state, w, None)?
            {
                workers.insert((id, None), item);
            }
        }
    }
    c.sources.push(source(
        "workers",
        None,
        stamp(state)?,
        workers.len(),
        more || nodes_more,
        (!installation).then_some("association_from_retained_attempts"),
    )?);
    c.attention.extend(workers.into_values());
    c.running = (!unknown_running).then(|| total(running, more));
    if unknown_running {
        c.sources.push(unavailable(
            "running",
            "process_start_observation_unavailable",
        ));
    }
    c.recovering = Some(total(recovering, more));
    Ok(c)
}

async fn reviews(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    projects: &[Project],
    truncated: bool,
) -> Result<Contribution> {
    let owner = runtime.reviews.as_ref().ok_or(Error::Unavailable)?;
    let mut c = Contribution::default();
    let mut n = 0;
    let mut more = truncated;
    for p in projects {
        let page = owner
            .inbox_lean(
                user.id,
                &crate::reviews::InboxQuery {
                    project_id: Some(p.label.id),
                    state: crate::reviews::InboxState::Open,
                    order: crate::reviews::InboxOrder::Decision,
                    limit: 25,
                    cursor: None,
                },
            )
            .await?;
        more |= page.next_cursor.is_some() || page.sampled;
        c.sources.push(source(
            "reviews",
            Some(p.label.clone()),
            page.server_time_ms,
            page.items.len(),
            page.next_cursor.is_some() || page.sampled || truncated,
            Some("sampled_code_review_decisions; decision_transition_time_unavailable"),
        )?);
        for item in page.items {
            if item.project_id != p.label.id {
                return Err(Error::Unavailable);
            }
            if item.eligible_owner {
                n += 1;
            }
            c.attention.push(attention(
                AttentionKind::Decision,
                reference(
                    "decision",
                    item.decision_id,
                    Revision(item.decision_revision.to_string()).into(),
                ),
                Some(p.label.clone()),
                "Waiting decision",
                if item.eligible_owner {
                    "An eligible owner can review this pending decision"
                } else {
                    "Your requested decision is pending"
                },
                None,
            ));
        }
    }
    if projects.is_empty() {
        c.sources
            .push(source("reviews", None, stamp(state)?, 0, truncated, None)?);
    }
    // This is the observed eligible-decision count, never a claim about questions.
    c.needs_you = Some(total(n, more));
    Ok(c)
}

async fn results(
    state: &AppState,
    user: &User,
    projects: &[Project],
    truncated: bool,
    mounted: bool,
) -> Result<Contribution> {
    if !mounted {
        return Err(Error::Unavailable);
    }
    let mut c = Contribution::default();
    for p in projects {
        let page = Box::pin(human_results::read_as(
            state,
            user,
            &human_results::Page {
                profile: Some(human_results::ReadProfile::RecordedNativeV2),
                project_id: Some(p.label.id),
                after: None,
                limit: Some(25),
            },
            None,
        ))
        .await?;
        c.sources.push(source(
            "results",
            Some(p.label.clone()),
            page.server_time_ms,
            page.items.len(),
            page.next_after.is_some() || truncated,
            Some("recorded_native_results_only"),
        )?);
        for r in page.items {
            if r.project_id != p.label.id {
                return Err(Error::Unavailable);
            }
            let revision = digest(&r.revision)?;
            let at = timestamp(r.recorded_at_ms)?;
            c.outcomes.push(RecentOutcome {
                subject: TitledRef {
                    kind: "result".into(),
                    id: r.report_id,
                    revision: Some(revision.clone()),
                    title: "Recorded process result".into(),
                },
                project: p.label.clone(),
                outcome: format!("process_{}", r.process_outcome),
                at: at.clone(),
            });
            if r.current_report_head && r.process_outcome == "failed" {
                c.attention.push(attention(AttentionKind::Failed,reference("result",r.report_id,revision),Some(p.label.clone()),"Failed process result","The current retained result reports process failure; task settlement is separate",Some(at)));
            }
        }
    }
    if projects.is_empty() {
        c.sources.push(source(
            "results",
            None,
            stamp(state)?,
            0,
            truncated,
            Some("recorded_native_results_only"),
        )?);
    }
    Ok(c)
}
async fn settlements(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    projects: &[Project],
    truncated: bool,
) -> Result<Contribution> {
    let mut c = Contribution::default();
    let Some(reviews) = &runtime.reviews else {
        c.sources
            .push(unavailable("settlement", "review_owner_not_mounted"));
        return Ok(c);
    };
    let owner = settlement::Service::new(state.clone(), reviews.as_ref().clone());
    let mut transaction = tx(runtime).await?;
    let ids: Vec<String> = projects.iter().map(|p| p.label.id.to_string()).collect();
    let rows=sqlx::query("SELECT s.operation_id,s.receipt_digest,l.execution_spec#>>'{lifecycle,project_id}' AS project FROM profiled_settlement_evaluations s JOIN profiled_launches l USING(attempt_id) WHERE s.actor_id=$1 AND l.execution_spec#>>'{lifecycle,project_id}'=ANY($2) ORDER BY s.created_at_ms DESC,s.operation_id LIMIT 26").bind(user.id).bind(ids).fetch_all(&mut *transaction).await?;
    transaction.commit().await?;
    for row in rows.iter().take(25) {
        let receipt = owner.lookup(user, row.get("operation_id")).await?;
        let project = projects
            .iter()
            .find(|p| p.label.id.to_string() == row.get::<String, _>("project"))
            .ok_or(Error::Unavailable)?;
        // Owner lookup verifies this immutable evaluation's retained receipt digest.
        // Acceptance does not establish Graph task completion.
        let outcome = match receipt.disposition {
            settlement::EvaluationDisposition::Accepted => "settlement_accepted",
            settlement::EvaluationDisposition::Rejected => "settlement_rejected",
            settlement::EvaluationDisposition::Stale => "settlement_stale",
            settlement::EvaluationDisposition::NeedsReconciliation => {
                "settlement_needs_reconciliation"
            }
        };
        c.outcomes.push(RecentOutcome {
            subject: TitledRef {
                kind: "settlement_evaluation".into(),
                id: receipt.operation_id,
                revision: Some(digest(&row.get::<String, _>("receipt_digest"))?),
                title: "Settlement evaluation".into(),
            },
            project: project.label.clone(),
            outcome: outcome.into(),
            at: timestamp(receipt.recorded_at_ms)?,
        });
    }
    c.sources.push(source(
        "settlement",
        None,
        stamp(state)?,
        rows.len().min(25),
        truncated || rows.len() > 25,
        None,
    )?);
    Ok(c)
}

async fn operations(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    scope: &Scope,
    labels: &BTreeMap<Uuid, ProjectRef>,
    truncated: bool,
) -> Result<Contribution> {
    let mut transaction = tx(runtime).await?;
    let rows: Vec<Uuid> = sqlx::query_scalar("SELECT operation_id FROM dashboard_operations WHERE principal_id=$1 AND retired_at IS NULL AND disposition IN ('pending','unknown') AND ($2::uuid IS NULL OR scope->>'project_id'=$2::text) ORDER BY created_at,operation_id LIMIT 101").bind(user.id).bind(scope.project()).fetch_all(&mut *transaction).await?;
    transaction.commit().await?;
    let mut c = Contribution::default();
    for id in rows.iter().take(MEMBERS) {
        let op = actions::journal::lookup(&runtime.pool, user.id, *id)
            .await
            .map_err(|_| Error::Unavailable)?;
        if !matches!(
            op.disposition,
            actions::Disposition::Pending | actions::Disposition::Unknown
        ) {
            continue;
        }
        let project = op.scope.project().and_then(|id| labels.get(&id).cloned());
        if op.scope.project().is_some() && project.is_none() {
            continue;
        }
        c.attention.push(attention(
            AttentionKind::UncertainOperation,
            reference(
                "operation",
                op.operation_id,
                digest(
                    op.request_digest
                        .strip_prefix("sha256:")
                        .ok_or(Error::Unavailable)?,
                )?,
            ),
            project,
            "Operation awaiting confirmation",
            "Look up the original operation before attempting another action",
            Some(Timestamp(op.created_at)),
        ));
    }
    c.sources.push(source(
        "operations",
        None,
        stamp(state)?,
        c.attention.len(),
        truncated || rows.len() > MEMBERS,
        Some("dashboard_journal_only"),
    )?);
    Ok(c)
}
