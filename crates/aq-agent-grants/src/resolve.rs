//! Where an attempt's grants come from (§6.3), resolved at offer and sealed
//! with the launch:
//!
//! 1. **Profile.** The coding default (§6.6) plus what the profile's
//!    `effective_permissions` request in this vocabulary.
//! 2. **Project ceiling.** Intersected with the project's ceiling, when the
//!    project policy sets one.
//! 3. **Owner additions.** Project-level additions the owner configured
//!    (owner decision 6: per-task overrides come later). Only an owner can
//!    widen beyond the default this way.
//! 4. **Inheritance.** For a task an agent created, intersected with the
//!    creating attempt's grants, so a child never holds more than its creator.
use super::{Grant, GrantSet, Limits, Scope, op};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Owner decision 1: an agent creates at most 5 tasks per attempt, at most
/// 2 levels below its own task, and they are schedulable at once.
pub const DEFAULT_MAX_CREATED_TASKS: u32 = 5;
pub const DEFAULT_MAX_DEPTH: u32 = 2;

/// The `aq.coding` default (§6.6, owner decisions 1–4).
pub fn default_coding() -> GrantSet {
    use Scope::*;
    let g = Grant::new;
    GrantSet::new(vec![
        g(op::TASK_READ, ProjectSelf),
        g(op::COMMENT_READ, ProjectSelf),
        g(op::KNOWLEDGE_READ, ProjectSelf),
        g(op::RESULT_READ, ProjectSelf),
        g(op::TASK_CREATE_SUBTASK, TaskSubtree).limited(Limits {
            max_created_tasks: Some(DEFAULT_MAX_CREATED_TASKS),
            max_depth: Some(DEFAULT_MAX_DEPTH),
            draft_only: false,
        }),
        g(op::COMMENT_WRITE, TaskSubtree),
        g(op::KNOWLEDGE_AUTHOR, ProjectSelf),
        g(op::QUESTION_ASK, AttemptSelf),
        g(op::QUESTION_READ, AttemptSelf),
        g(op::WAIT_REGISTER, AttemptSelf),
        g(op::WAIT_READ, AttemptSelf),
        g(op::WAIT_ACK, AttemptSelf),
        // Owner decision 4: the supervisor and the agent's own task tree.
        g(op::MESSAGE_SEND, TaskSubtree),
        g(op::MESSAGE_READ, TaskSubtree),
        g(op::MESSAGE_ACK, TaskSubtree),
    ])
    .expect("static default grants")
}

/// A project's agent-grant policy (`ProjectPolicy.agent_grants`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectGrants {
    /// The most any attempt in the project holds. Lowering it narrows live
    /// attempts at their next call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<Vec<Grant>>,
    /// Owner additions for the project, such as `task.create.project`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<Grant>,
    /// Draft task authority for work created by a playbook.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_work_kind: BTreeMap<String, WorkKindGrants>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkKindGrants {
    pub max_created_tasks: u32,
    #[serde(default)]
    pub edit_drafts: bool,
}
impl ProjectGrants {
    pub fn validate(&self) -> Result<(), super::InvalidGrant> {
        if let Some(c) = &self.ceiling {
            GrantSet::new(c.clone())?;
        }
        if self
            .by_work_kind
            .keys()
            .any(|kind| kind.trim().is_empty() || kind.len() > 128)
        {
            return Err(super::InvalidGrant("invalid work kind".into()));
        }
        GrantSet::new(self.add.clone()).map(|_| ())
    }
    pub fn work_kind(&self, kind: Option<&str>) -> Option<WorkKindGrants> {
        self.by_work_kind.get(kind?).copied()
    }

    pub fn ceiling(&self) -> Option<GrantSet> {
        self.ceiling.clone().and_then(|c| GrantSet::new(c).ok())
    }
}

/// One aq-profiles permission: `operation` at `scope_reference`
/// (`project:<id>`, `attempt:self`, `repository:<id>`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission<'a> {
    pub operation: &'a str,
    pub scope_reference: &'a str,
}
/// The grants a profile's effective permissions request. Only operations in
/// the vocabulary count; `wait.show` is `wait.read` and `wait.checkpoint` is
/// part of `wait.register`. A `project:<id>` naming the attempt's own project
/// is `project:self`; any other project is dropped (only an owner override
/// reaches another project).
pub fn from_permissions(permissions: &[Permission<'_>], project: Uuid) -> Vec<Grant> {
    let own = format!("project:{project}");
    let mut out: Vec<Grant> = Vec::new();
    for p in permissions {
        let operation = match p.operation {
            "wait.show" => op::WAIT_READ,
            "wait.checkpoint" => op::WAIT_REGISTER,
            o => match op::ALL.iter().find(|a| **a == o) {
                Some(a) => *a,
                None => continue,
            },
        };
        let scope = if p.scope_reference == own {
            Scope::ProjectSelf
        } else {
            match p.scope_reference.parse::<Scope>() {
                Ok(Scope::Project(_)) | Err(_) => continue,
                Ok(s) => s,
            }
        };
        merge(&mut out, Grant::new(operation, scope));
    }
    out
}
/// Add `g`, keeping the wider scope when the operation is already present.
fn merge(set: &mut Vec<Grant>, g: Grant) {
    match set.iter_mut().find(|x| x.operation == g.operation) {
        Some(x) if x.scope.within(g.scope) => x.scope = g.scope,
        Some(_) => {}
        None => set.push(g),
    }
}

impl WorkKindGrants {
    fn apply(self, mut grants: Vec<Grant>, sealing: bool) -> Vec<Grant> {
        let creation = matches!(self.max_created_tasks, 1..);
        let editing = |g: &Grant| matches!(g.operation.as_str(), op::TASK_EDIT | op::TASK_CANCEL);
        grants.retain(|g| match g.operation.as_str() {
            op::TASK_CREATE_PROJECT => false,
            op::TASK_CREATE_SUBTASK => creation && (sealing || g.limits.draft_only),
            _ => !editing(g) || self.edit_drafts,
        });
        for grant in &mut grants {
            if grant.operation == op::TASK_CREATE_SUBTASK {
                grant.scope = Scope::TaskSubtree;
                if sealing {
                    grant.limits.max_created_tasks = Some(self.max_created_tasks);
                }
                grant.limits = grant.limits.meet(Limits {
                    max_created_tasks: Some(self.max_created_tasks),
                    max_depth: Some(DEFAULT_MAX_DEPTH),
                    draft_only: true,
                });
            } else if editing(grant) {
                grant.scope = Scope::TaskSubtree;
            }
        }
        if sealing && self.edit_drafts {
            for operation in [op::TASK_EDIT, op::TASK_CANCEL] {
                if !grants.iter().any(|grant| grant.operation == operation) {
                    grants.push(Grant::new(operation, Scope::TaskSubtree));
                }
            }
        }
        grants
    }

    /// A policy change can reduce a live attempt's task authority.
    pub fn narrow(self, sealed: &GrantSet) -> GrantSet {
        let requested =
            GrantSet::new(self.apply(sealed.grants().to_vec(), false)).unwrap_or_default();
        sealed.intersect(&requested)
    }
}

/// The sealed grant set for one attempt.
pub fn resolve(profile: &[Grant], project: &ProjectGrants, creator: Option<&GrantSet>) -> GrantSet {
    resolve_work(profile, project, creator, None)
}

pub fn resolve_work(
    profile: &[Grant],
    project: &ProjectGrants,
    creator: Option<&GrantSet>,
    kind: Option<&str>,
) -> GrantSet {
    let mut requested = default_coding().grants().to_vec();
    for g in profile.iter().chain(&project.add) {
        match requested.iter_mut().find(|x| x.operation == g.operation) {
            // An addition with limits keeps them; the default's limits stay
            // when the profile only restates the operation.
            Some(x) if x.scope.within(g.scope) && x.scope != g.scope => {
                x.scope = g.scope;
                if !g.limits.is_empty() {
                    x.limits = g.limits;
                }
            }
            Some(_) => {}
            None => requested.push(g.clone()),
        }
    }
    if let Some(policy) = project.work_kind(kind) {
        requested = policy.apply(requested, true);
    }
    let mut set = GrantSet::new(requested).unwrap_or_default();
    if let Some(ceiling) = project.ceiling() {
        set = set.intersect(&ceiling);
    }
    if let Some(creator) = creator {
        set = set.intersect(creator);
    }
    set
}
