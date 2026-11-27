//! Bounded owner observations. None of these projections grants control or
//! promotes process results / settlement evaluations into Graph completion.
use super::{
    contract::*,
    service::{auth, envelope, fence, stamp, timestamp, tx},
    *,
};
use crate::{
    execution::{
        authority::{self, Policy},
        profiled::{Retention, Status},
    },
    profiled_execution::{human_results, settlement},
};
use aq_dashboard_contract::overview::{
    AttentionDescription, Contribution, OverviewSources, attention, total, unavailable,
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
struct SourceSample {
    at: u64,
    count: usize,
    truncated: bool,
}
fn source(
    name: &str,
    project: Option<ProjectRef>,
    sample: SourceSample,
    reason: Option<&str>,
) -> Result<SourceObservation> {
    let SourceSample {
        at,
        count,
        truncated,
    } = sample;
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
    let lowercase_hex = value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if value.len() != 64 || !lowercase_hex {
        return Err(Error::Unavailable);
    }
    Ok(ReferenceRevision::Digest(Digest(format!("sha256:{value}"))))
}
pub(super) async fn overview(
    request: &DashboardRequest,
    scope: Scope,
    mounted: MountedOwners,
) -> Result<Envelope<Overview>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        fence(state, runtime, user, &a).await?;
        return envelope(
            state,
            scope,
            Overview::hidden(),
            Some("owner_visibility_required"),
        );
    }
    let read = OverviewRead::load(request, &scope).await?;
    let mut data = read.collect(&mounted).await?;
    read.fence_projects().await?;
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

struct OverviewRead<'a> {
    request: &'a DashboardRequest,
    scope: &'a Scope,
    projects: Vec<Project>,
    ids: Vec<Uuid>,
    labels: BTreeMap<Uuid, ProjectRef>,
    truncated: bool,
}
impl<'a> OverviewRead<'a> {
    async fn load(request: &'a DashboardRequest, scope: &'a Scope) -> Result<Self> {
        let mut transaction = tx(&request.runtime).await?;
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
        Ok(Self {
            request,
            scope,
            projects,
            ids,
            labels,
            truncated,
        })
    }
    async fn fence_projects(&self) -> Result<()> {
        // Re-read bindings after all owner awaits. A different workspace/name/revision
        // must never be attached to observations made against the original binding.
        let mut transaction = tx(&self.request.runtime).await?;
        let latest = sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id").bind(&self.ids).fetch_all(&mut *transaction).await?;
        if latest.len() != self.projects.len()
            || latest.iter().any(|r| {
                self.projects
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
        Ok(())
    }
    fn empty_source(
        &self,
        contribution: &mut Contribution,
        name: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        if self.projects.is_empty() {
            let sample = SourceSample {
                at: stamp(&self.request.state)?,
                count: 0,
                truncated: self.truncated,
            };
            contribution
                .sources
                .push(source(name, None, sample, reason)?);
        }
        Ok(())
    }
    async fn collect(&self, mounted: &MountedOwners) -> Result<Overview> {
        // SO-01: results holds node row locks for its whole page while attempts
        // locks each node with a 250 ms timeout. Run concurrently, attempts lost
        // that wait on ~40% of reads, so these two sources run in turn.
        let ((results, attempts), ready, reviews, settlements, operations, internal) = tokio::join!(
            Box::pin(async {
                let results = bounded_source("results", self.results(mounted.results)).await;
                // Another request (a sessions or terminal read) may still hold one
                // node briefly: retry once within the same source budget.
                let all = self.scope.project().is_none();
                let attempts = bounded_source("attempts_and_workers", async {
                    match self.attempts(all).await {
                        Err(_) => self.attempts(all).await,
                        done => done,
                    }
                })
                .await;
                (results, attempts)
            }),
            Box::pin(bounded_source("graph_ready", self.ready(mounted.graph))),
            Box::pin(bounded_source("reviews", self.reviews())),
            Box::pin(bounded_source("settlement", self.settlements())),
            Box::pin(bounded_source("operations", self.operations())),
            Box::pin(bounded_source("internal_work", self.internal_work())),
        );
        let sources = OverviewSources {
            attempts,
            ready,
            reviews,
            results,
            settlements,
            operations,
            internal,
        };
        let mut data = sources.overview(
            self.projects.iter().map(|p| p.label.clone()),
            self.truncated,
        );
        super::attention::filter(&self.request.runtime, self.request.user.id, &mut data).await?;
        Ok(data)
    }
}

mod activity;
mod graph;
mod owners;
