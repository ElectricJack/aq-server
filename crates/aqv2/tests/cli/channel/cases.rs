use super::*;
pub(super) const REQUEST: &str = "@REQUEST";
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}
fn channel_tasks() -> Vec<Vec<String>> {
    vec![
        args(&["whoami"]),
        args(&["commands"]),
        args(&["commands", "--all"]),
        args(&["task", "show"]),
        args(&["task", "show", OTHER_TASK]),
        args(&["task", "children"]),
        args(&[
            "task",
            "history",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
        args(&[
            "task",
            "create",
            "--title",
            "Verify changelog links",
            "--parent",
            "self",
        ]),
        args(&[
            "task",
            "create",
            "--title",
            "Draft one",
            "--body",
            "Body",
            "--acceptance-criteria",
            "Done",
            "--label",
            "docs",
            "--parent",
            OTHER_TASK,
            "--draft",
            "--priority",
            "50",
        ]),
        args(&["task", "create", "--title", "Unrelated top-level task"]),
        args(&[
            "task",
            "graph",
            "show",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
    ]
}
fn channel_collaboration() -> Vec<Vec<String>> {
    vec![
        args(&["knowledge", "search", "release checklist"]),
        args(&[
            "knowledge",
            "search",
            "release",
            "--kind",
            "knowledge",
            "--limit",
            "5",
            "--cursor",
            "c1",
        ]),
        args(&["knowledge", "show", RECORD]),
        args(&[
            "knowledge",
            "author",
            "--title",
            "Release checklist",
            "--body",
            "Run it.",
            "--label",
            "ops",
            "--content-type",
            "procedure",
        ]),
        args(&["comment", "add", "Found the checklist; following it."]),
        args(&["comment", "list", "--limit", "10"]),
        args(&["comment", "list", "--task", OTHER_TASK]),
        args(&[
            "comment",
            "edit",
            COMMENT,
            "--body",
            "Edited",
            "--revision",
            "2",
        ]),
        args(&["comment", "delete", COMMENT, "--revision", "3"]),
    ]
}
fn channel_mail() -> Vec<Vec<String>> {
    vec![
        args(&[
            "question",
            "ask",
            "Should the notes include the deprecated API?",
        ]),
        args(&["question", "show", DECISION]),
        args(&["wait", "register", "--request", REQUEST]),
        args(&["wait", "inspect", "--request", REQUEST]),
        args(&["wait", "ack", "--request", REQUEST]),
        args(&["wait", "list"]),
        args(&["wait", "show", WAIT]),
        args(&["message", "send", "Status: halfway"]),
        args(&[
            "message",
            "send",
            "Need a review",
            "--to",
            &format!("task:{OTHER_TASK}"),
        ]),
        args(&[
            "message",
            "send",
            "Question for you",
            "--to",
            &format!("supervisor:{CONVERSATION}"),
        ]),
        args(&["message", "send", "Hello, supervisor", "--to", "supervisor"]),
        args(&["message", "inbox", "--to", "session"]),
        args(&["message", "inbox"]),
        args(&["message", "body", MESSAGE]),
        args(&["message", "ack", MESSAGE, "--read"]),
    ]
}
fn channel_operations() -> Vec<Vec<String>> {
    vec![
        args(&["operation", "show", OPERATION]),
        args(&["operations", "show", OPERATION]),
        args(&[
            "run",
            "task.history",
            "--param",
            &format!("task_id={OTHER_TASK}"),
        ]),
        args(&[
            "run",
            "task.cancel",
            "--subject",
            &format!("task:{OTHER_TASK}:4"),
            "--reason",
            "duplicate",
        ]),
        args(&[
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
        ]),
        args(&[
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--limit",
            "3",
            "--before",
            "12.2",
        ]),
    ]
}

fn owner_tasks() -> Vec<Vec<String>> {
    vec![
        args(&["whoami"]),
        args(&["task", "show", OTHER_TASK, "--project", PROJECT]),
        args(&[
            "task",
            "create",
            "--title",
            "Write launch notes",
            "--project",
            PROJECT,
            "--expected-project-revision",
            "3",
            "--parent",
            OTHER_TASK,
            "--label",
            "docs",
        ]),
    ]
}
fn owner_collaboration() -> Vec<Vec<String>> {
    vec![
        args(&["knowledge", "search", "deploy", "--kind", "knowledge"]),
        args(&["knowledge", "show", RECORD, "--project", PROJECT]),
        args(&[
            "knowledge",
            "author",
            "--title",
            "Deploy runbook",
            "--body",
            "Run the migration first.",
            "--label",
            "ops",
            "--content-type",
            "procedure",
            "--project",
            PROJECT,
            "--expected-project-revision",
            "1",
        ]),
        args(&[
            "comment",
            "add",
            "Please also cover phones.",
            "--task",
            OTHER_TASK,
            "--project",
            PROJECT,
        ]),
        args(&[
            "comment",
            "list",
            "--task",
            OTHER_TASK,
            "--project",
            PROJECT,
        ]),
        args(&[
            "comment",
            "edit",
            COMMENT,
            "--body",
            "Edited",
            "--revision",
            "1",
            "--project",
            PROJECT,
        ]),
        args(&[
            "comment",
            "delete",
            COMMENT,
            "--revision",
            "2",
            "--project",
            PROJECT,
        ]),
    ]
}
fn owner_mail() -> Vec<Vec<String>> {
    vec![
        args(&["question", "show", DECISION]),
        args(&[
            "message",
            "send",
            "Hello",
            "--to",
            &format!("supervisor:{CONVERSATION}"),
        ]),
    ]
}
fn owner_operations() -> Vec<Vec<String>> {
    vec![
        args(&[
            "run",
            "task.cancel",
            "--subject",
            &format!("task:{OTHER_TASK}:4"),
            "--param",
            &format!("project_id={PROJECT}"),
            "--param",
            "expected_version=4",
            "--reason",
            "duplicate",
        ]),
        args(&[
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--limit",
            "16",
            "--after",
            "0",
        ]),
        args(&[
            "conversation",
            "messages",
            "--conversation-id",
            CONVERSATION,
            "--before",
            "7",
        ]),
    ]
}

pub(super) fn for_mode(mode: aqv2::Mode) -> Vec<Vec<String>> {
    let cases = match mode {
        aqv2::Mode::Channel => [
            channel_tasks(),
            channel_collaboration(),
            channel_mail(),
            channel_operations(),
        ],
        aqv2::Mode::Owner => [
            owner_tasks(),
            owner_collaboration(),
            owner_mail(),
            owner_operations(),
        ],
    };
    cases.into_iter().flatten().collect()
}
