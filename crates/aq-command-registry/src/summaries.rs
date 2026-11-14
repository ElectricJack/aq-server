//! Summaries of the action kinds that have no catalog entry. An `ActionKind`
//! declares its kind, grant and target in aq-server; what it does is said
//! here, where discovery reads it. Add a line when an action kind is added
//! (`commands::every_command_has_its_own_summary` names a missing one).

/// The summary an action kind gets until it has a line in [`action_summary`].
pub const PLACEHOLDER_SUMMARY: &str = "Dashboard action (protocol/dashboard-actions/v1)";

pub fn action_summary(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "comment.revise" => {
            "Add a corrected revision to a result comment (earlier revisions are kept)"
        }
        "decision.resolve" => {
            "Resolve a review decision: approve or reject its subject, with an optional comment"
        }
        "feedback.link" => "Link a result comment thread to an existing task",
        "feedback.task_create" => "Create a task from a result comment thread",
        "knowledge.author" => "Create an unverified knowledge record in a project",
        "knowledge.correct" => "Edit a knowledge record's title, body, labels or content type",
        "knowledge.restore" => "Restore a knowledge record to an earlier revision",
        "knowledge.retire" => "Retire a knowledge record, with a reason and an optional successor",
        "knowledge.verify" => {
            "Record a verified or disputed verdict on a knowledge record, with evidence"
        }
        "playbook.activation.activate" => {
            "Make an installed playbook artifact the active one on a binding"
        }
        "playbook.activation.rollback" => "Restore a binding's previous playbook activation",
        "playbook.run.cancel" => "Request cancellation of a playbook run",
        "playbook.run.start" => "Start a run of the active playbook on a binding",
        "question.answer" => "Answer an agent's question to the owner",
        "result.comment" => "Comment on a result revision",
        "result.evidence_waive" => "Waive a result's missing evidence, with a reason",
        "result.follow" => "Follow or unfollow a result",
        "session.message.send" => "Send a message to a running agent session's mailbox",
        "session.nudge" => {
            "Nudge a running agent session with a message, delivered at its next safe boundary"
        }
        "supervisor.message.send" => "Send a message to the supervisor conversation",
        "swarm.availability.set" => "Turn the owner's availability switch on or off",
        "task.activate" => "Make a draft task schedulable",
        "task.cancel" => "Cancel a task and stop any attempt holding it",
        "task.delete" => "Delete a task and its relationships; its history is kept",
        "task.dependency.add" => "Make one task block another",
        "task.dependency.remove" => "Remove a blocking dependency between two tasks",
        "task.edit" => "Edit a task's title, body, acceptance criteria or labels",
        "task.reopen" => {
            "Make a completed or canceled task schedulable again, if it never had an attempt"
        }
        "task.reprioritize" => "Set a task's priority (higher runs first)",
        "task.retry" => "Release a task whose attempt has settled for a replacement attempt",
        "thread.resolve" => {
            "Resolve a result comment thread, naming the follow-up revision and a reason"
        }
        "worker.drain" => "Stop a machine taking new work and let running attempts finish",
        "worker.revoke" => "Revoke every node of a machine (idempotent)",
        "worker.start" => "Let a machine take work again",
        "worker.stop" => "Stop a machine and hand off the attempts running on it",
        _ => return None,
    })
}
