//! How an action's subject follows from a command's params.
//!
//! The action union takes `{subject:{type,id,revision?}, expected_versions,
//! payload}` and every `ActionKind` refuses a subject that disagrees with its
//! payload. So the subject is redundant for a caller that names the thing it
//! changes: `forward` builds it from the params by the rule here, and a
//! request may still carry an explicit `subject`. Each rule states what the
//! kind's `authorize` checks; `commands::subject_rules_match_the_action_schemas`
//! (aq-server) holds the param names to the payload schemas.
use serde_json::{Value, json};

/// Where the subject's id comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    /// A payload field: it stays in the payload.
    Payload(&'static str),
    /// A param the payload does not have: taken out before forwarding.
    Param(&'static str),
    /// The kind has one subject.
    Fixed(&'static str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubjectRule {
    /// `subject.type`.
    pub entity: &'static str,
    pub id: Id,
    /// The payload field whose value is `subject.revision`, and the
    /// `expected_versions` key it is also sent under. `None`: the subject
    /// carries no revision and `expected_versions` is empty.
    pub revision: Option<(&'static str, &'static str)>,
}
impl SubjectRule {
    pub fn describe(&self) -> Value {
        let mut v = json!({"type":self.entity});
        match self.id {
            Id::Payload(p) => v["id_param"] = json!(p),
            Id::Param(p) => {
                v["id_param"] = json!(p);
                v["id_param_schema"] = json!({"type":"string","format":"uuid"});
            }
            Id::Fixed(id) => v["id"] = json!(id),
        }
        if let Some((param, key)) = self.revision {
            v["revision_param"] = json!(param);
            v["version_key"] = json!(key);
        }
        v
    }
}
const fn rule(
    entity: &'static str,
    id: Id,
    revision: Option<(&'static str, &'static str)>,
) -> SubjectRule {
    SubjectRule {
        entity,
        id,
        revision,
    }
}

/// The subject rule of an action kind.
pub fn subject(kind: &str) -> Option<SubjectRule> {
    const VERSION: &str = "expected_version";
    let task = rule("task", Id::Param("task_id"), Some((VERSION, "task")));
    let record = rule(
        "knowledge",
        Id::Param("node_id"),
        Some((VERSION, "knowledge")),
    );
    let binding = rule(
        "playbook_binding",
        Id::Payload("binding"),
        Some((VERSION, "playbook_binding")),
    );
    let machine = rule("machine", Id::Payload("machine"), None);
    let session = rule(
        "session",
        Id::Param("session_id"),
        Some(("expected_generation", "session")),
    );
    let comment = rule(
        "task_comment",
        Id::Param("comment_id"),
        Some(("expected_revision", "task_comment")),
    );
    // A result action checks its thread or assessment version, not the result's.
    let result = |id, key| rule("result", id, Some((VERSION, key)));
    Some(match kind {
        // agent_access/human/graph_actions.rs::admit; task_execution.rs
        "task.activate"
        | "task.reopen"
        | "task.cancel"
        | "task.delete"
        | "task.retry"
        | "task.edit"
        | "task.reprioritize"
        | "task.dependency.add"
        | "task.dependency.remove" => task,
        "knowledge.correct" | "knowledge.verify" | "knowledge.retire" | "knowledge.restore" => {
            record
        }
        "knowledge.author" => rule(
            "project",
            Id::Payload("project_id"),
            Some(("expected_project_revision", "project")),
        ),
        // agent_access/human/enqueue.rs
        "task.graph.create" => rule(
            "project",
            Id::Payload("project_id"),
            Some((VERSION, "project")),
        ),
        // agent_access/human/commands/action.rs: the payload names no project
        "task.create" => rule(
            "project",
            Id::Param("project_id"),
            Some(("expected_project_revision", "project")),
        ),
        // agent_access/human/comments.rs::target
        "task.comment.add" => rule("task", Id::Param("task_id"), None),
        "task.comment.edit" | "task.comment.delete" => comment,
        // results_feedback/actions.rs (ENTITY and VERSION_KEY); store.rs::subject
        "result.follow" => result(Id::Param("result_id"), "result"),
        "result.comment" => result(Id::Payload("result_id"), "result"),
        "result.evidence_waive" => result(Id::Param("result_id"), "assessment"),
        "feedback.task_create" | "feedback.link" => result(Id::Payload("result_id"), "thread"),
        "comment.revise" => rule(
            "comment",
            Id::Param("comment_id"),
            Some((VERSION, "comment")),
        ),
        "thread.resolve" => rule("thread", Id::Param("thread_id"), Some((VERSION, "thread"))),
        // reviews/action.rs; agent_access/questions/action.rs
        "decision.resolve" => rule(
            "decision",
            Id::Payload("decision_id"),
            Some(("expected_decision_revision", "decision")),
        ),
        "question.answer" => rule(
            "decision",
            Id::Param("decision_id"),
            Some(("expected_revision", "decision")),
        ),
        // agent_access/messaging/session_action.rs; global/writes/action.rs
        "session.message.send" | "session.nudge" => session,
        "supervisor.message.send" => rule(
            "conversation",
            Id::Param("conversation_id"),
            Some(("expected_conversation_revision", "conversation")),
        ),
        // playbooks/actions.rs
        "playbook.run.cancel" => rule("playbook_run", Id::Payload("run_id"), None),
        "playbook.run.start" | "playbook.activation.activate" | "playbook.activation.rollback" => {
            binding
        }
        // machine_actions.rs::bound; intelligence/availability_action.rs
        "worker.start" | "worker.stop" | "worker.drain" | "worker.revoke" => machine,
        "swarm.availability.set" => rule("swarm", Id::Fixed("owner-availability"), None),
        _ => return None,
    })
}
