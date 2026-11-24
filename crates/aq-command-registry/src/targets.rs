//! Resolve a command's grant target in the caller's project and task context.
use super::*;
use aq_agent_grants::{Holder, Relation, Target};
impl AgentScope {
    pub fn for_attempt(ids: aq_agent_grants::AttemptIds, relation: Relation) -> Self {
        Self {
            holder: Holder {
                project: ids.project_id,
            },
            own_task: ids.task_id,
            relation,
        }
    }
    /// The agent's scope target for `rule` (§6.4). Params gain defaults from
    /// the binding: an omitted `project_id` is the attempt's project and is
    /// removed before forwarding (agent routes take it from the binding), an
    /// omitted task param is the attempt's task, and a task equal to the
    /// attempt's task is `Own`. `relation` is the server's resolution of any
    /// other named task (its ancestry), or of a `Comment` rule's comment.
    pub fn target(
        &self,
        rule: TargetRule,
        params: &mut Map<String, Value>,
    ) -> Result<Target, BuildError> {
        Ok(match rule {
            TargetRule::Own => Target::OwnAttempt,
            TargetRule::OwnTask => self.task(self.own_task),
            TargetRule::Project(field) => Target::Project(self.project(params, field)?),
            TargetRule::Task(field) => self.named_task(params, field)?,
            TargetRule::Comment(field) => {
                params
                    .get(field)
                    .and_then(id)
                    .ok_or(BuildError("comment id must be a UUID"))?;
                Target::Task {
                    project: self.holder.project,
                    relation: self.relation,
                }
            }
            TargetRule::ParentTask { field, .. } => self.child(params.get(field))?,
            TargetRule::Mailbox(field) => self.mailbox(params.get(field))?,
            TargetRule::Subject => return Err(BuildError("this command takes no agent subject")),
        })
    }
}
fn id(v: &Value) -> Option<Uuid> {
    v.as_str()?.parse().ok()
}
/// The caller and graph relation used for command authorization.
#[derive(Clone, Copy)]
pub struct AgentScope {
    pub holder: Holder,
    pub own_task: Uuid,
    pub relation: Relation,
}
impl AgentScope {
    fn task(&self, task: Uuid) -> Target {
        Target::Task {
            project: self.holder.project,
            relation: if task == self.own_task {
                Relation::Own
            } else {
                self.relation
            },
        }
    }
    fn project(&self, params: &mut Map<String, Value>, field: &str) -> Result<Uuid, BuildError> {
        match params.remove(field) {
            None => Ok(self.holder.project),
            Some(v) => id(&v).ok_or(BuildError("project_id must be a UUID")),
        }
    }
    fn named_task(
        &self,
        params: &mut Map<String, Value>,
        field: &str,
    ) -> Result<Target, BuildError> {
        let p = self.project(params, "project_id")?;
        let t = match params.get(field) {
            None => self.own_task,
            Some(v) => id(v).ok_or(BuildError("task id must be a UUID"))?,
        };
        params.insert(field.into(), json!(t));
        Ok(if p != self.holder.project {
            Target::Project(p)
        } else {
            self.task(t)
        })
    }
    fn child(&self, parent: Option<&Value>) -> Result<Target, BuildError> {
        let parent = match parent {
            Some(Value::Null) | None => return Ok(Target::Project(self.holder.project)),
            Some(value) if value == "self" => self.own_task,
            Some(value) => id(value).ok_or(BuildError("parent_id must be a UUID or \"self\""))?,
        };
        let parent_relation = if parent == self.own_task {
            Relation::Own
        } else {
            self.relation
        };
        // The new child sits one level below its parent.
        let relation = match parent_relation {
            Relation::Own => Relation::Descendant(1),
            Relation::Descendant(depth) => Relation::Descendant(depth + 1),
            other => other,
        };
        Ok(Target::Task {
            project: self.holder.project,
            relation,
        })
    }
    fn mailbox(&self, recipient: Option<&Value>) -> Result<Target, BuildError> {
        let r = recipient.ok_or(BuildError("a mailbox command needs a recipient"))?;
        if r["kind"] != "task" {
            return Ok(Target::OwnAttempt);
        }
        let task = id(&r["task_id"]).ok_or(BuildError("task mailbox needs a task_id"))?;
        Ok(if r["project_id"] != json!(self.holder.project) {
            Target::Project(id(&r["project_id"]).unwrap_or_default())
        } else {
            self.task(task)
        })
    }
}
