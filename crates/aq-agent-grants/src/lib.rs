//! Agent grants (spec 0006 §6): the `(operation, scope)` vocabulary an attempt
//! holds, the scope check, and the structured refusal every agent-facing
//! surface returns. This crate is pure: it never reads a database or a
//! request. aq-server resolves the facts (the target's project and its task
//! ancestry) and asks this crate for the decision.
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub mod budget;
pub mod draft_scope;
pub mod refusal;
pub mod resolve;
#[cfg(feature = "store")]
pub mod store;
pub use refusal::{CODES, Refusal, RefusalCode};

/// The operations a grant can name (§6.2). Owner-only operations are listed
/// separately in [`OWNER_ONLY`] and can never be held by an agent.
pub mod op {
    pub const TASK_READ: &str = "task.read";
    pub const TASK_CREATE_SUBTASK: &str = "task.create.subtask";
    pub const TASK_CREATE_PROJECT: &str = "task.create.project";
    pub const TASK_EDIT: &str = "task.edit";
    pub const TASK_CANCEL: &str = "task.cancel";
    pub const COMMENT_READ: &str = "comment.read";
    pub const COMMENT_WRITE: &str = "comment.write";
    pub const KNOWLEDGE_READ: &str = "knowledge.read";
    pub const KNOWLEDGE_AUTHOR: &str = "knowledge.author";
    pub const QUESTION_ASK: &str = "question.ask";
    pub const QUESTION_READ: &str = "question.read";
    pub const WAIT_REGISTER: &str = "wait.register";
    pub const WAIT_READ: &str = "wait.read";
    pub const WAIT_ACK: &str = "wait.ack";
    pub const MESSAGE_SEND: &str = "message.send";
    pub const MESSAGE_READ: &str = "message.read";
    pub const MESSAGE_ACK: &str = "message.ack";
    pub const RESULT_READ: &str = "result.read";
    /// Spec 0017 §6.2: build a recipe from the task's pushed head and run a command in it.
    #[cfg(feature = "project_environments")]
    pub const ENVIRONMENT_TRY: &str = "environment.try";
    /// Every grantable operation, in the order the spec lists them.
    pub const ALL: [&str; 18 + cfg!(feature = "project_environments") as usize] = [
        TASK_READ,
        TASK_CREATE_SUBTASK,
        TASK_CREATE_PROJECT,
        TASK_EDIT,
        TASK_CANCEL,
        COMMENT_READ,
        COMMENT_WRITE,
        KNOWLEDGE_READ,
        KNOWLEDGE_AUTHOR,
        QUESTION_ASK,
        QUESTION_READ,
        WAIT_REGISTER,
        WAIT_READ,
        WAIT_ACK,
        MESSAGE_SEND,
        MESSAGE_READ,
        MESSAGE_ACK,
        RESULT_READ,
        #[cfg(feature = "project_environments")]
        ENVIRONMENT_TRY,
    ];
}

/// Operations no agent can hold (§6.2). Answers, decisions, verification,
/// waivers, playbook control, administration and grant changes stay with the
/// owner, so nothing an agent writes can approve its own work.
pub const OWNER_ONLY: [&str; 10] = [
    "question.answer",
    "decision.resolve",
    "knowledge.verify",
    "knowledge.retire",
    "result.evidence_waive",
    "playbook.activate",
    "playbook.run",
    "worker.admin",
    "project.admin",
    "grant.admin",
];
pub fn owner_only(operation: &str) -> bool {
    OWNER_ONLY.contains(&operation)
}
pub fn grantable(operation: &str) -> bool {
    op::ALL.contains(&operation)
}

/// Where a grant applies. Serialized as `attempt:self`, `task:self`,
/// `task:subtree`, `project:self` or `project:<uuid>` (owner override only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    AttemptSelf,
    TaskSelf,
    TaskSubtree,
    ProjectSelf,
    Project(Uuid),
}
impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttemptSelf => f.write_str("attempt:self"),
            Self::TaskSelf => f.write_str("task:self"),
            Self::TaskSubtree => f.write_str("task:subtree"),
            Self::ProjectSelf => f.write_str("project:self"),
            Self::Project(id) => write!(f, "project:{id}"),
        }
    }
}
impl std::str::FromStr for Scope {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "attempt:self" => Self::AttemptSelf,
            "task:self" => Self::TaskSelf,
            "task:subtree" => Self::TaskSubtree,
            "project:self" => Self::ProjectSelf,
            _ => {
                let id = s.strip_prefix("project:").ok_or(())?;
                // Only the canonical hyphenated lowercase form names a project.
                let id = Uuid::parse_str(id).ok().filter(|u| u.to_string() == id);
                Self::Project(id.filter(|u| !u.is_nil()).ok_or(())?)
            }
        })
    }
}
impl Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse()
            .map_err(|_| serde::de::Error::custom("unknown grant scope"))
    }
}
impl Scope {
    /// Breadth order used when two grants of one operation are held: the
    /// wider one is reported first and a narrowing keeps the narrower.
    fn breadth(self) -> u8 {
        match self {
            Self::AttemptSelf => 0,
            Self::TaskSelf => 1,
            Self::TaskSubtree => 2,
            Self::ProjectSelf => 3,
            Self::Project(_) => 4,
        }
    }
    /// True when every target this scope covers is also covered by `other`
    /// (for the same attempt). `project:<id>` is only within itself.
    pub fn within(self, other: Scope) -> bool {
        match (self, other) {
            (Self::Project(a), Self::Project(b)) => a == b,
            (Self::Project(_), _) | (_, Self::Project(_)) => false,
            (a, b) => a.breadth() <= b.breadth(),
        }
    }
}

/// Bounds a grant can carry (§6.6, owner decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Tasks this attempt may create under this grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_created_tasks: Option<u32>,
    /// Depth below the attempt's task a created task may sit at (1 = child).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Only draft tasks may be created with this grant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft_only: bool,
}
impl Limits {
    pub fn is_empty(&self) -> bool {
        self.max_created_tasks.is_none() && self.max_depth.is_none() && !self.draft_only
    }
    /// The tighter of two limit sets.
    pub fn meet(self, other: Limits) -> Limits {
        fn min(a: Option<u32>, b: Option<u32>) -> Option<u32> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        Limits {
            max_created_tasks: min(self.max_created_tasks, other.max_created_tasks),
            max_depth: min(self.max_depth, other.max_depth),
            draft_only: self.draft_only || other.draft_only,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub operation: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
}
impl Grant {
    pub fn new(operation: &str, scope: Scope) -> Self {
        Self {
            operation: operation.into(),
            scope,
            limits: Limits::default(),
        }
    }
    pub fn limited(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}

/// The grants one attempt holds, sorted and deduplicated by operation (one
/// scope per operation), so equal sets have equal digests.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantSet(Vec<Grant>);
pub const MAX_GRANTS: usize = 64;
impl GrantSet {
    /// Refuses unknown, owner-only and duplicate operations, and more than
    /// [`MAX_GRANTS`] entries.
    pub fn new(mut grants: Vec<Grant>) -> Result<Self, InvalidGrant> {
        if grants.len() > MAX_GRANTS {
            return Err(InvalidGrant("too many grants".into()));
        }
        grants.sort_by(|a, b| a.operation.cmp(&b.operation));
        for pair in grants.windows(2) {
            if pair[0].operation == pair[1].operation {
                return Err(InvalidGrant(format!("duplicate {}", pair[0].operation)));
            }
        }
        if let Some(g) = grants.iter().find(|g| !grantable(&g.operation)) {
            return Err(InvalidGrant(format!("{} is not grantable", g.operation)));
        }
        Ok(Self(grants))
    }
    pub fn empty() -> Self {
        Self(Vec::new())
    }
    pub fn grants(&self) -> &[Grant] {
        &self.0
    }
    pub fn get(&self, operation: &str) -> Option<&Grant> {
        self.0.iter().find(|g| g.operation == operation)
    }
    /// Whether an agent reads a Graph edge of type `kind`: the launch
    /// profile's edge types, plus `parent-child` edges whose parent `task.read`
    /// reaches, given as (the attempt's own task, a task created below it);
    /// `parent: None` checks the type only. A native profile lists only the
    /// edge types of its startup projection, so without this `task.children`
    /// could not read the subtasks an attempt creates (spec 0006 §6.2).
    pub fn edge_readable(
        &self,
        profile_types: Option<&Vec<serde_json::Value>>,
        kind: &serde_json::Value,
        parent: Option<(bool, bool)>,
    ) -> bool {
        if profile_types.is_some_and(|t| t.contains(kind)) {
            return true;
        }
        let scope = self.get(op::TASK_READ).map(|g| &g.scope);
        kind == "parent-child"
            && match (scope, parent) {
                (None, _) => false,
                (Some(_), None) | (Some(Scope::ProjectSelf | Scope::Project(_)), _) => true,
                (Some(Scope::TaskSubtree), Some((own, below))) => own || below,
                (Some(Scope::TaskSelf), Some((own, _))) => own,
                _ => false,
            }
    }
    /// `task.read` at `task:subtree`: reads need the tasks created below.
    pub fn reads_subtree(&self) -> bool {
        self.get(op::TASK_READ)
            .is_some_and(|g| g.scope == Scope::TaskSubtree)
    }
    /// Grants of the same family (`task.create.*` for `task.create.project`),
    /// for a refusal's `held` list. Never reveals anything but the caller's
    /// own authority.
    pub fn related(&self, operation: &str) -> Vec<Grant> {
        let family = operation.split('.').next().unwrap_or(operation);
        self.0
            .iter()
            .filter(|g| g.operation.split('.').next() == Some(family))
            .cloned()
            .collect()
    }
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&self.0).expect("grant set encodes");
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }
    /// Keep only what both sets allow: an operation in both, at the narrower
    /// scope and the tighter limits. A child never exceeds its creator.
    pub fn intersect(&self, ceiling: &GrantSet) -> GrantSet {
        let mut out = Vec::new();
        for g in &self.0 {
            let Some(c) = ceiling.get(&g.operation) else {
                continue;
            };
            // A draft-only ceiling cannot grant batch access to a caller
            // whose underlying grant allows only the legacy create route.
            if !g.limits.draft_only && c.limits.draft_only {
                continue;
            }
            let scope = if g.scope.within(c.scope) {
                g.scope
            } else if c.scope.within(g.scope) {
                c.scope
            } else {
                continue;
            };
            out.push(Grant {
                operation: g.operation.clone(),
                scope,
                limits: g.limits.meet(c.limits),
            });
        }
        GrantSet(out)
    }
    /// Remove the named operations.
    pub fn without(&self, operations: &[String]) -> GrantSet {
        GrantSet(
            self.0
                .iter()
                .filter(|g| !operations.contains(&g.operation))
                .cloned()
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidGrant(pub String);
impl fmt::Display for InvalidGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid grant: {}", self.0)
    }
}
impl std::error::Error for InvalidGrant {}

/// What a command touches, as resolved by the server against the attempt's
/// binding. `Task` carries the relation to the attempt's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The caller's own attempt (its questions, waits and mail).
    OwnAttempt,
    /// A task in `project`, related to the attempt's task as `relation`.
    Task { project: Uuid, relation: Relation },
    /// A project-wide target (a list, a search, a top-level create).
    Project(Uuid),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The attempt's own task.
    Own,
    /// A descendant of the attempt's task, at this depth (1 = child).
    Descendant(u32),
    /// Any other task in the project.
    Other,
}

/// The attempt facts a scope is evaluated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub project: Uuid,
}

/// `allows(grant, target)` (§6.4). Always pinned to the holder's project
/// unless the grant names another project (an owner override).
pub fn allows(scope: Scope, holder: Holder, target: Target) -> bool {
    match (scope, target) {
        (_, Target::OwnAttempt) => true,
        (Scope::Project(p), Target::Task { project, .. } | Target::Project(project)) => {
            p == project
        }
        (Scope::ProjectSelf, Target::Task { project, .. } | Target::Project(project)) => {
            project == holder.project
        }
        (Scope::AttemptSelf, _) => false,
        (_, Target::Project(_)) => false,
        (Scope::TaskSelf, Target::Task { project, relation }) => {
            project == holder.project && relation == Relation::Own
        }
        (Scope::TaskSubtree, Target::Task { project, relation }) => {
            project == holder.project && relation != Relation::Other
        }
    }
}

/// The outcome of checking one command against a grant set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(Grant),
    Deny(Refusal),
}
/// Check `operation` for `command` against `grants` and the resolved target.
pub fn check(
    grants: &GrantSet,
    holder: Holder,
    command: &str,
    operation: &str,
    target: Target,
) -> Decision {
    // Anything outside the grantable vocabulary is owner-only: deny by default.
    if owner_only(operation) || !grantable(operation) {
        return Decision::Deny(Refusal::owner_only(command, operation));
    }
    match grants.get(operation) {
        None => Decision::Deny(Refusal::grant_missing(
            command,
            operation,
            grants.related(operation),
        )),
        Some(g) if !allows(g.scope, holder, target) => Decision::Deny(Refusal::scope_denied(
            command,
            operation,
            grants.related(operation),
        )),
        Some(g) => Decision::Allow(g.clone()),
    }
}

/// Grant each `/v1/agent/*` question, comment, wait and result route requires, with
/// the registry command it maps to (spec 0006 §9). The graph and message
/// routes keep checking their agent-access-1 profile capability.
pub const AGENT_ROUTE_GRANTS: [(&str, &str, &str);
    14 + 2 * cfg!(feature = "project_environments") as usize] = [
    ("/v1/agent/questions", "question.ask", op::QUESTION_ASK),
    (
        "/v1/agent/questions/show",
        "question.show",
        op::QUESTION_READ,
    ),
    ("/v1/agent/comments", "task.comment.add", op::COMMENT_WRITE),
    ("/v1/agent/comments/list", "comment.list", op::COMMENT_READ),
    (
        "/v1/agent/comments/edit",
        "task.comment.edit",
        op::COMMENT_WRITE,
    ),
    (
        "/v1/agent/comments/delete",
        "task.comment.delete",
        op::COMMENT_WRITE,
    ),
    (
        "/v1/agent/waits/register",
        "wait.register",
        op::WAIT_REGISTER,
    ),
    ("/v1/agent/waits/inspect", "wait.inspect", op::WAIT_READ),
    ("/v1/agent/waits/ack", "wait.ack", op::WAIT_ACK),
    ("/v1/agent/waits/list", "wait.list", op::WAIT_READ),
    ("/v1/agent/waits/show", "wait.show", op::WAIT_READ),
    (
        "/v1/agent/results/list",
        "result.review.list",
        op::RESULT_READ,
    ),
    (
        "/v1/agent/results/show",
        "result.review.show",
        op::RESULT_READ,
    ),
    (
        "/v1/agent/results/patch",
        "result.review.patch",
        op::RESULT_READ,
    ),
    #[cfg(feature = "project_environments")]
    (
        "/v1/agent/environments/try",
        "environment.try",
        op::ENVIRONMENT_TRY,
    ),
    #[cfg(feature = "project_environments")]
    (
        "/v1/agent/environments/try/{job_id}",
        "environment.job.show",
        op::ENVIRONMENT_TRY,
    ),
];
/// `(command, grant, target)` for an agent route path. Comments act on a
/// task thread related to the attempt's task as `relation` (its own task
/// unless a comment list names another); questions and waits on the attempt.
pub fn route_grant(
    path: &str,
    holder: Holder,
    relation: Relation,
) -> Option<(&'static str, &'static str, Target)> {
    let (_, command, grant) = AGENT_ROUTE_GRANTS.iter().find(|(p, _, _)| *p == path)?;
    let target = if grant.starts_with("comment.") {
        Target::Task {
            project: holder.project,
            relation,
        }
    } else if *grant == op::RESULT_READ || grant.starts_with("environment.") {
        // Results are read, and environments tried, by project:
        // `project:self` reaches them and no narrower scope does.
        Target::Project(holder.project)
    } else {
        Target::OwnAttempt
    };
    Some((command, grant, target))
}

impl Target {
    /// The audit form. It names only the caller's own project and the
    /// relation, never another record's identity.
    pub fn audit(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::OwnAttempt => json!({"kind":"own_attempt"}),
            Self::Task { project, relation } => {
                json!({"kind":"task","project_id":project,"relation":match relation {
                    Relation::Own => "own".to_string(),
                    Relation::Descendant(d) => format!("descendant:{d}"),
                    Relation::Other => "other".to_string(),
                }})
            }
            Self::Project(p) => json!({"kind":"project","project_id":p}),
        }
    }
}

/// One `command_audit` row (§6.7): every authorization decision, allow or deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEntry {
    pub caller_kind: &'static str,
    pub caller_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub command: String,
    pub grant: Option<String>,
    pub target: serde_json::Value,
    pub allowed: bool,
    pub reason_code: Option<&'static str>,
    pub operation_id: Option<Uuid>,
    pub request_id: Uuid,
}
/// The grant an audit row names: the one actually required (a refusal's
/// `grant`) or used (an agent's matched grant), not the command's default.
/// `task.create` defaults to `task.create.subtask`, but a top-level create
/// needs `task.create.project`, and its refusal and audit row say so.
fn audited_grant(agent: bool, default: Option<&str>, decision: &Decision) -> Option<String> {
    match decision {
        Decision::Deny(r) if r.grant.is_some() => r.grant.clone(),
        Decision::Allow(g) if agent && default.is_some() => Some(g.operation.clone()),
        _ => default.map(Into::into),
    }
}
/// The attempt a decision was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptIds {
    pub attempt_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
}
impl AuditEntry {
    /// Any caller's decision; `ids` is the attempt for an agent.
    #[allow(clippy::too_many_arguments)]
    pub fn caller(
        caller_kind: &'static str,
        caller_id: Uuid,
        ids: Option<AttemptIds>,
        command: &str,
        grant: Option<&str>,
        target: Option<&Target>,
        decision: &Decision,
        operation_id: Option<Uuid>,
        request_id: Uuid,
    ) -> Self {
        Self {
            caller_kind,
            caller_id,
            attempt_id: ids.map(|i| i.attempt_id),
            task_id: ids.map(|i| i.task_id),
            project_id: ids.map(|i| i.project_id),
            command: command.into(),
            grant: audited_grant(ids.is_some(), grant, decision),
            target: target.map_or(serde_json::json!({}), Target::audit),
            allowed: matches!(decision, Decision::Allow(_)),
            reason_code: match decision {
                Decision::Allow(_) => None,
                Decision::Deny(r) => Some(r.code.as_str()),
            },
            operation_id,
            request_id,
        }
    }
    pub fn agent(
        ids: AttemptIds,
        command: &str,
        grant: &str,
        target: Target,
        decision: &Decision,
        request_id: Uuid,
    ) -> Self {
        Self {
            caller_kind: "agent",
            caller_id: ids.attempt_id,
            attempt_id: Some(ids.attempt_id),
            task_id: Some(ids.task_id),
            project_id: Some(ids.project_id),
            command: command.into(),
            grant: Some(grant.into()),
            target: target.audit(),
            allowed: matches!(decision, Decision::Allow(_)),
            reason_code: match decision {
                Decision::Allow(_) => None,
                Decision::Deny(r) => Some(r.code.as_str()),
            },
            operation_id: None,
            request_id,
        }
    }
}

#[cfg(test)]
mod tests;
