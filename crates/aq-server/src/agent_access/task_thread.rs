//! Spec 0006 §7: the task-thread block of the startup context. The rows are
//! read here; `aq-task-thread` renders and bounds them.
use super::*;
use aq_task_thread as thread;

fn at(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(String::new, |t| t.to_rfc3339())
}
/// The task-thread block of a startup text and the identity it is recorded under.
pub(crate) struct StartupInput {
    pub identity: String,
    pub text: String,
}
/// The block for `task`, as an optional startup input after the assignment,
/// or `None` when the task has no comments, questions or unread mail.
pub(crate) async fn startup_input(
    state: &AppState,
    project: Uuid,
    task: Uuid,
    kind: Option<&str>,
) -> Option<StartupInput> {
    let pool = &state.store.pool;
    let max = thread::MAX_ITEMS as u16;
    let (comments, more) = aq_task_comments::list(
        pool,
        aq_task_comments::Thread {
            project_id: project,
            task_id: task,
        },
        None,
        max,
    )
    .await
    .ok()?;
    let questions: Vec<(Uuid, String, String, Option<String>)> = sqlx::query_as("SELECT decision_id,state,question,answer FROM question_decisions WHERE project_id=$1 AND task_id=$2 ORDER BY created_at_ms DESC LIMIT $3")
        .bind(project).bind(task).bind(max as i64).fetch_all(pool).await.ok()?;
    let mail: Vec<(Value, String, i64)> = sqlx::query_as("SELECT sender,body,created_at_ms FROM mailbox_messages m WHERE recipient->>'kind'='task' AND recipient->>'project_id'=$1 AND recipient->>'task_id'=$2 AND NOT EXISTS(SELECT 1 FROM mailbox_receipts r WHERE r.message_id=m.message_id AND r.disposition='read') ORDER BY created_at_ms DESC LIMIT $3")
        .bind(project.to_string()).bind(task.to_string()).bind(max as i64).fetch_all(pool).await.ok()?;
    let creator = aq_agent_grants::store::creator(pool, task).await.ok()?;
    let creator = creator.map(|v| grant_check::sealed(Some(v)).unwrap_or_default());
    let grants = aq_agent_grants::resolve::resolve_work(
        &[],
        &grant_check::project_grants(state, project),
        creator.as_ref(),
        kind,
    );
    let t = thread::Thread {
        comments: comments
            .iter()
            .filter_map(|c| {
                let author = match &c.author {
                    aq_task_comments::Author::Agent { attempt_id, .. } => {
                        format!("agent (attempt {attempt_id})")
                    }
                    a => a.kind().to_string(),
                };
                Some(thread::Comment {
                    at: c.created_at.to_rfc3339(),
                    author,
                    body: c.body.clone()?,
                })
            })
            .collect(),
        more_comments: more.is_some(),
        questions: questions.into_iter().map(Into::into).collect(),
        messages: mail
            .into_iter()
            .map(|(sender, body, ms)| thread::Message {
                at: at(ms),
                sender: sender["kind"].as_str().unwrap_or("unknown").into(),
                body,
            })
            .collect(),
        commands: crate::commands::agent_commands(&grants),
    };
    if t.comments.is_empty() && t.questions.is_empty() && t.messages.is_empty() {
        return None;
    }
    let text = thread::render(&t, thread::BUDGET_BYTES)?;
    let digest = aq_admission::Digest::of(text.as_bytes());
    Some(StartupInput {
        identity: format!("aq.task-thread/{}", digest.as_str()),
        text,
    })
}
