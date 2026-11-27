//! Combine bounded owner contributions into the dashboard overview, retaining unknown coverage.
use crate::*;
use std::collections::BTreeMap;
#[derive(Default)]
pub struct Contribution {
    pub sources: Vec<SourceObservation>,
    pub attention: Vec<AttentionItem>,
    pub outcomes: Vec<RecentOutcome>,
    pub running: Option<Total>,
    pub recovering: Option<Total>,
    pub ready: Option<Total>,
    pub needs_you: Option<Total>,
    pub per_project_running: BTreeMap<Uuid, u32>,
}
pub fn total(value: usize, truncated: bool) -> Total {
    Total {
        value: Some(value as u32),
        accuracy: if truncated {
            Accuracy::Estimated
        } else {
            Accuracy::Exact
        },
    }
}
pub fn unavailable(name: &str, reason: &str) -> SourceObservation {
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
pub struct AttentionDescription<'a> {
    pub title: &'a str,
    pub reason: &'a str,
    pub since: Option<Timestamp>,
}
pub fn attention(
    kind: AttentionKind,
    subject: EntityRef,
    project: Option<ProjectRef>,
    detail: AttentionDescription<'_>,
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
        title: detail.title.into(),
        reason: detail.reason.into(),
        impact: "Review the owning record for current context".into(),
        since: detail.since,
        action: None,
    }
}

pub struct OverviewSources {
    pub attempts: Contribution,
    pub ready: Contribution,
    pub reviews: Contribution,
    pub results: Contribution,
    pub settlements: Contribution,
    pub operations: Contribution,
    pub internal: Contribution,
}
impl Contribution {
    fn in_motion(
        &self,
        projects: impl IntoIterator<Item = ProjectRef>,
        truncated: bool,
        estimated: bool,
    ) -> Vec<InMotion> {
        projects
            .into_iter()
            .map(|p| InMotion {
                running: if self.running.is_some() {
                    total(
                        *self.per_project_running.get(&p.id).unwrap_or(&0) as usize,
                        truncated || estimated,
                    )
                } else {
                    Total::unknown()
                },
                project: p,
                next_constraint: None,
                last_progress_at: None,
            })
            .collect()
    }
}
impl OverviewSources {
    pub fn overview(
        self,
        projects: impl IntoIterator<Item = ProjectRef>,
        truncated: bool,
    ) -> Overview {
        let Self {
            attempts,
            ready,
            reviews,
            results,
            settlements,
            operations,
            internal,
        } = self;
        let counts = Counts {
            running: attempts.running.clone().unwrap_or_else(Total::unknown),
            recovering: attempts.recovering.clone().unwrap_or_else(Total::unknown),
            ready: ready.ready.clone().unwrap_or_else(Total::unknown),
            needs_you: reviews.needs_you.clone().unwrap_or_else(Total::unknown),
        };
        let in_motion = attempts.in_motion(
            projects,
            truncated,
            matches!(counts.running.accuracy, Accuracy::Estimated),
        );
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
        for c in [
            attempts,
            ready,
            reviews,
            results,
            settlements,
            operations,
            internal,
        ] {
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
        data
    }
}
impl Overview {
    pub fn hidden() -> Self {
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
        }
    }
}
