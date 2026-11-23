//! The host's side of the delivery commands in PostgreSQL: the request the
//! host freezes when it accepts a command, one step of a running operation,
//! the facts the steps leave behind (`delivery_pull_requests`,
//! `delivery_main_heads`) and where a project's work is delivered. The server
//! keeps only its own operation bookkeeping (the effect row, the operation row
//! and the completion event). Nothing here decides anything about a result.
use super::{
    Completion, Delivery, DeliveryTarget, GitTarget, PullRequestObserved, SocketGit, Step,
    bounded_text, bounded_wait_ms, merge_method, without_delivery, without_target,
};
use crate::DeliveryError;
use crate::github::{GitHubApp, GitHubAppConfig};
use crate::policy::{GitHubAdapter, GitHubRepository, UnconfiguredGitHub};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

/// The longest title and body the host sends with a pull request.
const TITLE_BYTES: usize = 256;
const BODY_BYTES: usize = 16 * 1024;

impl Delivery {
    /// The commands over the aq-git socket and the configured GitHub App.
    /// Without an App every forge call refuses, and says why.
    pub fn configured(git_socket: &Path, app: Option<&GitHubAppConfig>) -> Self {
        let github: Arc<dyn GitHubAdapter> = match GitHubApp::from_config(app) {
            Ok(app) => Arc::new(app),
            Err(_) => Arc::new(UnconfiguredGitHub),
        };
        Self::new(Arc::new(SocketGit::new(git_socket)), github)
    }
}

/// Where a bound project's work is delivered: the forge repository and the
/// default branch onboarding recorded for the binding's repository. `None`
/// when onboarding recorded neither.
pub async fn onboarded_target(
    pool: &PgPool,
    project: Uuid,
    git: GitTarget,
) -> Result<Option<DeliveryTarget>, sqlx::Error> {
    let Some(state) = crate::repository_onboarding::delivery_state(
        pool,
        project,
        git.repository_id,
        git.binding_generation,
    )
    .await?
    else {
        return Ok(None);
    };
    let github = &state["request"]["github"];
    let (Some(owner), Some(name), Some(branch)) = (
        github["owner"]
            .as_str()
            .filter(|s| crate::repository_onboarding::slug(s)),
        github["name"]
            .as_str()
            .filter(|s| crate::repository_onboarding::slug(s)),
        state["default_branch"].as_str(),
    ) else {
        return Ok(None);
    };
    Ok(Some(DeliveryTarget {
        git,
        github: GitHubRepository {
            owner: owner.into(),
            name: name.into(),
        },
        default_branch: branch.into(),
    }))
}

/// The title and the body of a task's pull request: the task's own title
/// (`AQ task <id>` without one) and its latest report's conclusion, bounded.
pub async fn pull_request_text(
    pool: &PgPool,
    task: Uuid,
    title: Option<&str>,
) -> Result<(String, String), sqlx::Error> {
    let conclusion: Option<String> = sqlx::query_scalar(
        "SELECT convert_from(r.report_bytes,'UTF8')::jsonb->>'conclusion' \
         FROM execution_attempts a JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id \
         JOIN profiled_result_reports r ON r.report_id=h.report_id \
         WHERE a.task_id=$1 \
         ORDER BY (convert_from(r.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint DESC,r.report_id DESC LIMIT 1",
    )
    .bind(task)
    .fetch_optional(pool)
    .await?
    .flatten();
    let title = title
        .filter(|t| !t.trim().is_empty())
        .map_or_else(|| format!("AQ task {task}"), str::to_owned);
    Ok((
        bounded_text(&title, TITLE_BYTES).replace(['\n', '\r'], " "),
        bounded_text(conclusion.as_deref().unwrap_or(""), BODY_BYTES).to_owned(),
    ))
}

/// Whom an accepted command is about, as the host resolved it.
#[derive(Debug, Clone)]
pub struct Accepting<'a> {
    pub command: &'a str,
    pub project: Uuid,
    /// `None` for `main.sync`, and for a task the host does not know.
    pub task: Option<Uuid>,
    /// The commit the task's result was retained at, or `""`.
    pub head: &'a str,
    /// The operation's subject, as the host names it.
    pub subject: Value,
    /// `pull_request.open`: the title and the body ([`pull_request_text`]).
    pub text: Option<(String, String)>,
    /// `pull_request.await_checks`: Settings
    /// `delivery-limits.longest_check_wait_ms`.
    pub longest_check_wait_ms: Option<u64>,
    /// `pull_request.await_checks`: Settings `delivery-limits.check_log_bytes`.
    pub check_log_bytes: Option<u64>,
    /// Check and merge operations pin the resolved target at acceptance.
    pub target: Option<DeliveryTarget>,
    /// The latest attempt/result for merge evidence.
    pub result_ids: Option<(Uuid, Uuid)>,
}

/// The request the host freezes when it accepts a command from the playbook's
/// `input`, which [`Delivery::step`] runs later; `Err(reason)` is the
/// catalog's refusal, answered at once instead.
pub async fn frozen_request(
    pool: &PgPool,
    accepting: Accepting<'_>,
    input: &Value,
    now_ms: i64,
) -> Result<Result<Value, &'static str>, DeliveryError> {
    let Accepting { command, task, .. } = accepting;
    let mut request = json!({
        "command": command,
        "project": accepting.project.to_string(),
        "task": task.map(|t| t.to_string()),
        "subject": accepting.subject,
    });
    let known: Option<(i64, String)> = match task {
        Some(task) => {
            sqlx::query_as("SELECT number,state FROM delivery_pull_requests WHERE task_id=$1")
                .bind(task)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    let number = |(n, _): &(i64, String)| u64::try_from(*n).unwrap_or(0);
    match command {
        "branch.publish" => {
            let Some(task) = task.filter(|_| !accepting.head.is_empty()) else {
                return Ok(Err("no_branch"));
            };
            // The lease: the head the last publication put on the forge.
            let last: Option<String> = sqlx::query_scalar("SELECT output->>'head' FROM playbook_host_operations WHERE command_name='branch.publish' AND status='published' AND subject_id=$1 ORDER BY completed_at_ms DESC LIMIT 1")
                .bind(task.to_string())
                .fetch_optional(pool)
                .await?
                .flatten();
            request["expected_old"] = json!(last);
            request["pull_request"] = json!(known.as_ref().map(number));
            request["result_head"] = json!(accepting.head);
        }
        "pull_request.open" => {
            let (title, body) = accepting
                .text
                .ok_or(DeliveryError::InvalidInput("pull request text"))?;
            request["title"] = json!(title);
            request["body"] = json!(body);
            request["pull_request"] = json!(known.as_ref().map(number));
        }
        "pull_request.merge" => {
            let Some(open) = known
                .as_ref()
                .filter(|(_, state)| state == "open" || state == "merged")
            else {
                return Ok(Err("no_pull_request"));
            };
            request["pull_request"] = json!(number(open));
            request["method"] = input["method"].clone();
            request["expected_head_sha"] = input["expected_head_sha"].clone();
            request["merge_target"] = serde_json::to_value(accepting.target)?;
            if request["expected_head_sha"].as_str() != Some(accepting.head) {
                return Ok(Err("head_changed"));
            }
            if let Some((attempt, result)) = accepting.result_ids {
                request["attempt_id"] = json!(attempt);
                request["result_id"] = json!(result);
            }
        }
        "pull_request.close" => {
            request["pull_request"] = json!(known.as_ref().map(number));
            request["result_head"] = json!(accepting.head);
        }
        "pull_request.await_checks" => {
            request["pull_request"] = json!(known.as_ref().map(number));
            request["result_head"] = json!(accepting.head);
            request["check_target"] = serde_json::to_value(accepting.target)?;
            let longest = accepting
                .longest_check_wait_ms
                .ok_or(DeliveryError::InvalidInput("longest_check_wait_ms"))?;
            let asked = input["deadline_ms"]
                .as_u64()
                .ok_or(DeliveryError::InvalidInput("deadline_ms"))?;
            // Frozen with the request, like the wait, so one operation reads
            // its logs under the limit it was accepted with.
            request["check_log_bytes"] = json!(accepting.check_log_bytes.unwrap_or(0));
            let wait = i64::try_from(bounded_wait_ms(asked, longest)).unwrap_or(i64::MAX);
            request["deadline_at_ms"] = json!(now_ms.saturating_add(wait));
            request["required"] = input["required"].clone();
        }
        _ => {}
    }
    Ok(Ok(request))
}

/// Marks at most `limit` running operations of `commands` in one namespace
/// generation as looked at now, the least recently looked at first, and
/// returns them: the driver's bounded pass.
pub async fn due_operations(
    conn: &mut PgConnection,
    namespace: Uuid,
    generation: i64,
    commands: &[&str],
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let commands: Vec<String> = commands.iter().map(|c| c.to_string()).collect();
    sqlx::query_scalar("UPDATE playbook_host_operations SET observed_at_ms=$4 WHERE operation_id IN (SELECT operation_id FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name=ANY($5) AND status='running' ORDER BY COALESCE(observed_at_ms,created_at_ms),operation_id LIMIT $3) RETURNING operation_id")
        .bind(namespace)
        .bind(generation)
        .bind(i64::from(limit))
        .bind(now_ms)
        .bind(&commands)
        .fetch_all(conn)
        .await
}

/// One step of a running operation from its frozen `request`: the command's
/// one completion, or `None` while it runs. `target` is where the project's
/// work is delivered (`None`: no longer bound), `delivery` the commands
/// (`None`: no forge or no central git configured). The pull request each
/// step sees and the head a sync finds are recorded for `work.read`.
pub async fn run_step(
    pool: &PgPool,
    delivery: Option<&Delivery>,
    target: Option<&DeliveryTarget>,
    operation: Uuid,
    command: &str,
    request: &Value,
    now_ms: i64,
) -> Result<Option<Completion>, DeliveryError> {
    let project = request_uuid(request, "project").ok_or(DeliveryError::InvalidInput("project"))?;
    let task = request_uuid(request, "task");
    let step = match (command, target, delivery) {
        ("pull_request.merge", target, delivery) => {
            let task = task.ok_or(DeliveryError::InvalidInput("task"))?;
            run_merge_step(
                pool,
                delivery,
                target,
                operation,
                (project, task),
                request,
                now_ms,
            )
            .await?
        }
        (_, None, _) => Some(without_target(command)),
        (_, Some(_), None) => without_delivery(command),
        (_, Some(target), Some(delivery)) => {
            let run = delivery
                .step(operation, command, request, target, now_ms)
                .await
                .ok_or(DeliveryError::InvalidInput("request"))?;
            if let (Some(task), Some(seen)) = (task, &run.observed) {
                record_pull_request(pool, task, seen, now_ms).await?;
            }
            run.step
        }
    };
    // A check wait with no forge keeps waiting until its deadline.
    let step = match (step, request["deadline_at_ms"].as_i64()) {
        (Some(step), _) => step,
        (None, Some(at)) if command == "pull_request.await_checks" && now_ms >= at => {
            Step::Done(Completion::new("timeout", json!({})))
        }
        (None, _) => return Ok(None),
    };
    let Step::Done(completion) = step else {
        return Ok(None);
    };
    if let (Some(task), Some(seen)) = (task, &completion.pull_request) {
        record_pull_request(pool, task, seen, now_ms).await?;
    }
    if let Some(head) = completion.output["head"]
        .as_str()
        .filter(|_| command == "main.sync")
    {
        record_main_head(pool, project, head, now_ms).await?;
    }
    Ok(Some(completion))
}

fn request_uuid(request: &Value, field: &str) -> Option<Uuid> {
    request[field]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
}

async fn run_merge_step(
    pool: &PgPool,
    delivery: Option<&Delivery>,
    target: Option<&DeliveryTarget>,
    operation: Uuid,
    (project, task): (Uuid, Uuid),
    request: &Value,
    now_ms: i64,
) -> Result<Option<Step>, DeliveryError> {
    const MERGE: &str = "pull_request.merge";
    let Some(pr) = request["pull_request"].as_u64() else {
        return Ok(Some(Step::Done(Completion::refused("no_pull_request"))));
    };
    let retained: Option<(Value, String)> = sqlx::query_as(
        "SELECT target,merge_sha FROM delivery_merge_evidence WHERE operation_id=$1",
    )
    .bind(operation)
    .fetch_optional(pool)
    .await?;
    let current = serde_json::to_value(target)?;
    if request.get("merge_target") == Some(&Value::Null) {
        return Ok(Some(without_target(MERGE)));
    }
    // Keep cleanup tied to the repository that merged, including old intents.
    let frozen = retained
        .as_ref()
        .map(|(target, _)| target)
        .or(request.get("merge_target"));
    if frozen.is_some_and(|target| target != &current) {
        return Ok(None);
    }
    let Some(target) = target else {
        return Ok(Some(without_target(MERGE)));
    };
    let Some(delivery) = delivery else {
        return Ok(if retained.is_some() {
            None
        } else {
            without_delivery(MERGE)
        });
    };
    if retained.is_none() {
        let method = request["method"]
            .as_str()
            .ok_or(DeliveryError::InvalidInput("method"))?;
        let head = request["expected_head_sha"]
            .as_str()
            .ok_or(DeliveryError::InvalidInput("expected_head_sha"))?;
        let sha = match delivery
            .merge_on_github(target, pr, merge_method(method), head)
            .await
        {
            super::MergeAttempt::Confirmed(sha) => sha,
            super::MergeAttempt::Unconfirmed(step) => return Ok(Some(step)),
        };
        let number = i64::try_from(pr).map_err(|_| DeliveryError::InvalidInput("number"))?;
        // Commit the merge before the separately retryable branch deletion.
        let inserted = sqlx::query(
            "INSERT INTO delivery_merge_evidence(\
             operation_id,project_id,task_id,attempt_id,result_id,pull_request,target,\
             target_ref,expected_head_sha,merge_sha,observed_at_ms) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation)
        .bind(project)
        .bind(task)
        .bind(request_uuid(request, "attempt_id"))
        .bind(request_uuid(request, "result_id"))
        .bind(number)
        .bind(&current)
        .bind(target.main_ref())
        .bind(head)
        .bind(sha)
        .bind(now_ms)
        .execute(pool)
        .await?;
        if inserted.rows_affected() == 0 {
            return Ok(None);
        }
    }
    Ok(Some(
        delivery
            .finish_merge(
                operation,
                target,
                task,
                pr,
                request["expected_head_sha"]
                    .as_str()
                    .ok_or(DeliveryError::InvalidInput("expected_head_sha"))?,
            )
            .await,
    ))
}

/// The pull request as the last command left it (`work.read`
/// `pull_request`). A command that did not read the url leaves the stored one.
async fn record_pull_request(
    pool: &PgPool,
    task: Uuid,
    seen: &PullRequestObserved,
    now_ms: i64,
) -> Result<(), DeliveryError> {
    let number = i64::try_from(seen.number).map_err(|_| DeliveryError::InvalidInput("number"))?;
    let failed = json!(seen.checks_failed);
    // The log tail replaces the last one with each look, so it rolls with the
    // pull request row and never outlives the head it was read for.
    let (log_head, log_tail) = match &seen.checks_log {
        Some(log) => (Some(log.head.as_str()), Some(log.tail.as_str())),
        None => (None, None),
    };
    let query = match &seen.url {
        Some(url) => sqlx::query("INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,checks_log_head,checks_log_tail,updated_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(task_id) DO UPDATE SET number=$2,url=$3,state=$4,mergeable=$5,checks_state=$6,checks_failed=$7,checks_log_head=$8,checks_log_tail=$9,updated_at_ms=$10")
            .bind(task).bind(number).bind(url.clone()).bind(seen.state).bind(seen.mergeable).bind(seen.checks_state).bind(failed).bind(log_head).bind(log_tail).bind(now_ms),
        None => sqlx::query("UPDATE delivery_pull_requests SET state=$3,mergeable=$4,checks_state=$5,checks_failed=$6,checks_log_head=$7,checks_log_tail=$8,updated_at_ms=$9 WHERE task_id=$1 AND number=$2")
            .bind(task).bind(number).bind(seen.state).bind(seen.mergeable).bind(seen.checks_state).bind(failed).bind(log_head).bind(log_tail).bind(now_ms),
    };
    query.execute(pool).await?;
    Ok(())
}

/// The commit main points at after a sync (`work.read` `main_head`).
async fn record_main_head(
    pool: &PgPool,
    project: Uuid,
    head: &str,
    now_ms: i64,
) -> Result<(), DeliveryError> {
    if head.len() != 40 || !super::object_id(head) {
        return Ok(());
    }
    sqlx::query("INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,$3) ON CONFLICT(project_id) DO UPDATE SET head=$2,observed_at_ms=$3")
        .bind(project)
        .bind(head)
        .bind(now_ms)
        .execute(pool)
        .await?;
    Ok(())
}
