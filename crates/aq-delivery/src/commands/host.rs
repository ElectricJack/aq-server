//! The host's side of the delivery commands in PostgreSQL: the request the
//! host freezes when it accepts a command, one step of a running operation,
//! the facts the steps leave behind (`delivery_pull_requests`,
//! `delivery_main_heads`) and where a project's work is delivered. The server
//! keeps only its own operation bookkeeping (the effect row, the operation row
//! and the completion event). Nothing here decides anything about a result.
use super::{
    Completion, Delivery, DeliveryTarget, FrozenCommand, GitTarget, MergeGuard,
    PullRequestObserved, Refreshed, Run, SocketGit, Step, TaskOperation, bounded_text,
    bounded_wait_ms, merge_method, without_delivery, without_target,
};
use crate::DeliveryError;
use crate::github::{GitHubApp, GitHubAppConfig};
use crate::policy::{GitHubAdapter, GitHubRepository, UnconfiguredGitHub};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use std::{path::Path, sync::Arc, time::Duration};
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

    /// The same commands, giving each aq-git call `deadline` (Settings
    /// `delivery-limits.git_transfer_timeout_ms`).
    pub fn git_waiting(&self, git_socket: &Path, deadline: Duration) -> Self {
        Self::new(
            Arc::new(SocketGit::new(git_socket).waiting(deadline)),
            self.github.clone(),
        )
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
    /// `pull_request.await_checks` and `pull_request.merge`: Settings
    /// `delivery-limits.check_log_bytes`.
    pub check_log_bytes: Option<u64>,
    /// `pull_request.merge`: Settings
    /// `delivery-limits.longest_merge_queue_wait_ms`.
    pub longest_merge_queue_wait_ms: Option<u64>,
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
            let last = published_head(pool, task).await?;
            request["expected_old"] = json!(last);
            if let Some(target) = &accepting.target {
                request["publication_target"] = serde_json::to_value(target)?;
            }
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
            if let Err(reason) =
                accepting.freeze_merge(&mut request, input, known.as_ref(), now_ms)?
            {
                return Ok(Err(reason));
            }
            request["pull_request_head"] = json!(accepted_head(pool, task, accepting.head).await?);
        }
        "pull_request.close" => {
            request["pull_request"] = json!(known.as_ref().map(number));
            request["result_head"] = json!(accepting.head);
            request["pull_request_head"] = json!(accepted_head(pool, task, accepting.head).await?);
        }
        "pull_request.await_checks" => {
            request["pull_request"] = json!(known.as_ref().map(number));
            accepting.freeze_wait(&mut request, input, now_ms)?;
        }
        _ => {}
    }
    Ok(Ok(request))
}

impl Accepting<'_> {
    fn freeze_wait(
        &self,
        request: &mut Value,
        input: &Value,
        now_ms: i64,
    ) -> Result<(), DeliveryError> {
        request["result_head"] = json!(self.head);
        request["check_target"] = serde_json::to_value(&self.target)?;
        let longest = self
            .longest_check_wait_ms
            .ok_or(DeliveryError::InvalidInput("longest_check_wait_ms"))?;
        let asked = input["deadline_ms"]
            .as_u64()
            .ok_or(DeliveryError::InvalidInput("deadline_ms"))?;
        // Frozen with the request, like the wait, so one operation reads
        // its logs under the limit it was accepted with.
        request["check_log_bytes"] = json!(self.check_log_bytes.unwrap_or(0));
        let wait = i64::try_from(bounded_wait_ms(asked, longest)).unwrap_or(i64::MAX);
        request["deadline_at_ms"] = json!(now_ms.saturating_add(wait));
        request["longest_check_wait_ms"] = json!(longest);
        request["required"] = input["required"].clone();
        Ok(())
    }
    fn freeze_merge(
        &self,
        request: &mut Value,
        input: &Value,
        known: Option<&(i64, String)>,
        now_ms: i64,
    ) -> Result<Result<(), &'static str>, DeliveryError> {
        let Some(open) = known.filter(|(_, state)| state == "open" || state == "merged") else {
            return Ok(Err("no_pull_request"));
        };
        request["pull_request"] = json!(u64::try_from(open.0).unwrap_or(0));
        request["method"] = input["method"].clone();
        request["expected_head_sha"] = input["expected_head_sha"].clone();
        request["merge_target"] = serde_json::to_value(&self.target)?;
        if request["expected_head_sha"].as_str() != Some(self.head) {
            return Ok(Err("head_changed"));
        }
        if let Some((attempt, result)) = self.result_ids {
            request["attempt_id"] = json!(attempt);
            request["result_id"] = json!(result);
        }
        // Frozen like a check wait, so one merge waits in a queue under the
        // limit it was accepted with.
        let wait = self
            .longest_merge_queue_wait_ms
            .ok_or(DeliveryError::InvalidInput("longest_merge_queue_wait_ms"))?;
        request["queued_since_ms"] = json!(now_ms);
        request["queue_deadline_at_ms"] =
            json!(now_ms.saturating_add(i64::try_from(wait).unwrap_or(i64::MAX)));
        request["check_log_bytes"] = json!(self.check_log_bytes.unwrap_or(0));
        Ok(Ok(()))
    }
}
pub struct DueOperations<'a> {
    pub namespace: Uuid,
    pub generation: i64,
    pub commands: &'a [&'a str],
    pub limit: u32,
    pub now_ms: i64,
}
#[derive(Clone, Copy)]
pub struct DeliveryRuntime<'a> {
    pub service: Option<&'a Delivery>,
    pub target: Option<&'a DeliveryTarget>,
}
/// The forge lease recorded by a previous successful publication of this
/// task: the head it published, or the head a recorded refresh moved it to.
pub async fn published_head<'c>(
    db: impl sqlx::PgExecutor<'c>,
    task: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    Ok(sqlx::query_scalar("SELECT COALESCE(r.merged_head,r.from_head,o.head) FROM (SELECT head FROM (SELECT output->>'head' AS head,to_timestamp(completed_at_ms::double precision/1000) AS recorded_at FROM playbook_host_operations WHERE command_name='branch.publish' AND status='published' AND subject_id=$1::text UNION ALL SELECT evidence#>>'{observation,head}',recorded_at FROM retained_publication_recoveries WHERE evidence#>>'{identity,task_id}'=$1::text AND evidence#>>'{observation,state}'='published') publications ORDER BY recorded_at DESC LIMIT 1) o LEFT JOIN delivery_branch_refreshes r ON r.task_id=$1 AND r.result_head=o.head")
        .bind(task)
        .fetch_optional(db)
        .await?
        .flatten())
}

/// The head the task's pull request is accepted at for result head `result`
/// when a recorded refresh moved it (`None`: the result head itself). The
/// wait step verifies each refreshed head with GitHub before it records it.
pub async fn accepted_head<'c>(
    db: impl sqlx::PgExecutor<'c>,
    task: Option<Uuid>,
    result: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(merged_head,from_head) FROM delivery_branch_refreshes WHERE task_id=$1 AND result_head=$2")
        .bind(task)
        .bind(result)
        .fetch_optional(db)
        .await
}

/// The request with the refresh recorded for its result head and, for a
/// wait, whether it may ask for one now. A repository refreshes one pull
/// request at a time, oldest queued first: another task's row holds the slot
/// while its pull request is open, its checks have not failed and either its
/// refresh or (queued earlier than this one) its last look is younger than
/// `longest_check_wait_ms`, and no decision on the task came after both. A
/// pending refresh of this task waits for GitHub. Nothing caps refreshes:
/// each needs green checks on the current head and a (head, base) pair not
/// asked for before, so they are bounded by how often main moves.
async fn with_refresh(
    db: &mut PgConnection,
    request: &Value,
    target: &DeliveryTarget,
    task: Uuid,
    now_ms: i64,
) -> Result<Value, DeliveryError> {
    let mut request = request.clone();
    let Some(result) = request["result_head"].as_str() else {
        return Ok(request);
    };
    let longest = request["longest_check_wait_ms"].as_i64();
    let since = now_ms.saturating_sub(longest.unwrap_or(0));
    type Row = (Option<String>, Option<String>, Option<String>, bool, bool);
    let (from, base, merged, pending, taken): Row = sqlx::query_as(
        "SELECT r.from_head,r.base_head,r.merged_head,
         COALESCE(r.merged_head IS NULL AND r.requested_at_ms>$3,false),
         EXISTS(SELECT 1 FROM delivery_branch_refreshes o JOIN delivery_pull_requests p ON p.task_id=o.task_id
           WHERE o.repository_id=$4 AND o.task_id<>$1 AND p.state='open' AND p.checks_state<>'failed'
           AND (o.requested_at_ms>$3 OR (o.seen_at_ms>$3
             AND (o.queued_at_ms,o.task_id)<(COALESCE(r.queued_at_ms,$5),$1)))
           AND NOT EXISTS(SELECT 1 FROM delivery_decisions d WHERE d.task_id=o.task_id
             AND d.decided_at_ms>=GREATEST(o.seen_at_ms,COALESCE(o.requested_at_ms,0))))
         FROM (SELECT $1::uuid AS task_id) t
         LEFT JOIN delivery_branch_refreshes r ON r.task_id=t.task_id AND r.result_head=$2",
    )
    .bind(task)
    .bind(result)
    .bind(since)
    .bind(target.git.repository_id)
    .bind(now_ms)
    .fetch_one(db)
    .await?;
    let allow = !pending && !taken;
    request["refresh"] = json!({
        "from": from, "base": base, "merged": merged, "allow": longest.map(|_| allow),
    });
    Ok(request)
}

/// Keeps what a wait did to its refresh: its place in the queue, a new
/// request (this task's slot) or GitHub's merge commit of it, checked
/// against its parents.
async fn record_refresh(
    db: &mut PgConnection,
    frozen: FrozenCommand<'_>,
    (project, task, target): (Uuid, Uuid, &DeliveryTarget),
    refreshed: &Refreshed,
    now_ms: i64,
) -> Result<(), DeliveryError> {
    let result = frozen.request["result_head"].as_str();
    let repository = target.git.repository_id;
    match refreshed {
        Refreshed::Queued(head) => sqlx::query("INSERT INTO delivery_branch_refreshes(task_id,project_id,repository_id,result_head,from_head,refreshes,queued_at_ms,seen_at_ms) VALUES($1,$2,$3,$4,$5,0,$6,$6) \
             ON CONFLICT(task_id) DO UPDATE SET seen_at_ms=$6")
            .bind(task).bind(project).bind(repository).bind(result).bind(head).bind(now_ms),
        Refreshed::Requested { from, base } => sqlx::query("INSERT INTO delivery_branch_refreshes(task_id,project_id,repository_id,result_head,from_head,base_head,refreshes,queued_at_ms,seen_at_ms,requested_at_ms) VALUES($1,$2,$3,$4,$5,$6,1,$7,$7,$7) \
             ON CONFLICT(task_id) DO UPDATE SET project_id=$2,repository_id=$3,result_head=$4,from_head=$5,base_head=$6,merged_head=NULL,seen_at_ms=$7,requested_at_ms=$7, \
             queued_at_ms=CASE WHEN delivery_branch_refreshes.result_head=$4 THEN delivery_branch_refreshes.queued_at_ms ELSE $7 END, \
             refreshes=CASE WHEN delivery_branch_refreshes.result_head=$4 THEN LEAST(delivery_branch_refreshes.refreshes+1,1000) ELSE 1 END")
            .bind(task).bind(project).bind(repository).bind(result).bind(from).bind(base).bind(now_ms),
        Refreshed::Merged(head) => sqlx::query("UPDATE delivery_branch_refreshes SET merged_head=$3 WHERE task_id=$1 AND result_head=$2 AND merged_head IS NULL")
            .bind(task).bind(result).bind(head),
    }
    .execute(db)
    .await?;
    Ok(())
}

/// Marks at most `limit` running operations of `commands` in one namespace
/// generation as looked at now, the least recently looked at first, and
/// returns them: the driver's bounded pass.
pub async fn due_operations(
    conn: &mut PgConnection,
    pass: DueOperations<'_>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let DueOperations {
        namespace,
        generation,
        commands,
        limit,
        now_ms,
    } = pass;
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
/// step sees and the head a sync finds are recorded for `work.read`. `db` is
/// the caller's transaction for this step: branch refreshes are decided and
/// recorded in it under a per-repository lock.
pub async fn run_step(
    pool: &PgPool,
    db: &mut PgConnection,
    runtime: DeliveryRuntime<'_>,
    frozen: FrozenCommand<'_>,
    now_ms: i64,
) -> Result<Option<Completion>, DeliveryError> {
    let FrozenCommand {
        operation,
        name: command,
        request,
    } = frozen;
    let DeliveryRuntime {
        service: delivery,
        target,
    } = runtime;
    let project = request_uuid(request, "project").ok_or(DeliveryError::InvalidInput("project"))?;
    let task = request_uuid(request, "task");
    let step = match (command, target, delivery) {
        ("pull_request.merge", _, _) => {
            let task = task.ok_or(DeliveryError::InvalidInput("task"))?;
            MergeStep {
                frozen,
                project,
                task,
                now_ms,
            }
            .run(pool, runtime)
            .await?
        }
        (_, None, _) => Some(without_target(command)),
        (_, Some(_), None) => without_delivery(command),
        (_, Some(target), Some(delivery)) => {
            if command == "branch.publish" {
                if request
                    .get("publication_target")
                    .is_some_and(|p| serde_json::to_value(target).ok().as_ref() != Some(p))
                {
                    return Ok(Some(Completion::refused("no_branch")));
                }
                let report =
                    crate::retained_publication::validate_target(pool, frozen, target).await?;
                if crate::retained_publication::dispatched(pool, operation, report).await? {
                    return Ok(Some(
                        crate::retained_publication::recover_step(db, request).await?,
                    ));
                }
            }
            let refreshing = matches!(command, "pull_request.await_checks" | "branch.publish");
            let with = match task.filter(|_| refreshing) {
                Some(task) => {
                    // Waits of one repository decide and record their refreshes one at a time.
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                        .bind(format!("aq.delivery.refresh/{}", target.git.repository_id))
                        .execute(&mut *db)
                        .await?;
                    Some(with_refresh(db, request, target, task, now_ms).await?)
                }
                None => None,
            };
            let prepared = FrozenCommand::new(operation, command, with.as_ref().unwrap_or(request));
            let run = if command == "branch.publish" {
                let task = task.ok_or(DeliveryError::InvalidInput("task"))?;
                let step = if let Some(mut step) = prepared.branch_preflight(delivery, target).await
                {
                    // Only a terminal read-only refusal proves this request
                    // never reached Git. Historical/uncertain requests remain fenced.
                    if let Step::Done(completion) = &mut step
                        && completion.outcome == "refused"
                    {
                        completion.diagnostic = Some("publication_not_dispatched");
                    }
                    step
                } else if !crate::retained_publication::reserve(pool, frozen, target).await? {
                    Step::Done(crate::retained_publication::recover_step(db, request).await?)
                } else {
                    delivery
                        .branch_publish(
                            TaskOperation::new(operation, target, task),
                            request["expected_old"].as_str(),
                            request["result_head"].as_str(),
                        )
                        .await
                };
                Run {
                    step: Some(step),
                    observed: None,
                    refreshed: None,
                }
            } else {
                delivery
                    .step(prepared, target, now_ms)
                    .await
                    .ok_or(DeliveryError::InvalidInput("request"))?
            };
            if let (Some(task), Some(refreshed)) = (task, &run.refreshed) {
                record_refresh(db, frozen, (project, task, target), refreshed, now_ms).await?;
            }
            if let (Some(task), Some(seen)) = (task, &run.observed) {
                record_pull_request(pool, task, seen, now_ms).await?;
            }
            run.step
        }
    };
    let Some(step) = completed_step(step, frozen, now_ms) else {
        return Ok(None);
    };
    let Step::Done(completion) = step else {
        return Ok(None);
    };
    if let (Some(task), Some(seen)) = (task, &completion.pull_request) {
        record_pull_request(pool, task, seen, now_ms).await?;
    }
    // A close or the publication of another head ends the branch's refresh.
    // A merge keeps it, so that a replay still accepts the merged head;
    // `work.complete` deletes it.
    if let ("pull_request.close", "closed") | ("branch.publish", "published") =
        (command, completion.outcome)
    {
        sqlx::query("DELETE FROM delivery_branch_refreshes WHERE task_id=$1 AND result_head IS DISTINCT FROM $2")
            .bind(task)
            .bind(completion.output["head"].as_str().filter(|_| command == "branch.publish"))
            .execute(pool)
            .await?;
    }
    if let Some(head) = completion.output["head"]
        .as_str()
        .filter(|_| command == "main.sync")
    {
        record_main_head(pool, project, head, now_ms).await?;
    }
    Ok(Some(completion))
}

fn completed_step(step: Option<Step>, frozen: FrozenCommand<'_>, now_ms: i64) -> Option<Step> {
    let FrozenCommand {
        name: command,
        request,
        ..
    } = frozen;
    // A check wait with no forge keeps waiting until its deadline.
    match (step, request["deadline_at_ms"].as_i64()) {
        (Some(step), _) => Some(step),
        (None, Some(at)) if command == "pull_request.await_checks" && now_ms >= at => {
            Some(Step::Done(Completion::new("timeout", json!({}))))
        }
        (None, _) => None,
    }
}
fn request_uuid(request: &Value, field: &str) -> Option<Uuid> {
    request[field]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
}

struct MergeStep<'a> {
    frozen: FrozenCommand<'a>,
    project: Uuid,
    task: Uuid,
    now_ms: i64,
}
struct MergeEvidence<'a> {
    current: &'a Value,
    number: i64,
    head: &'a str,
    sha: String,
}
impl MergeStep<'_> {
    /// The result head and the pull request head the merge is pinned to.
    fn heads(&self) -> Result<(&str, &str), DeliveryError> {
        let request = self.frozen.request;
        let result = request["expected_head_sha"]
            .as_str()
            .ok_or(DeliveryError::InvalidInput("expected_head_sha"))?;
        Ok((
            result,
            request["pull_request_head"].as_str().unwrap_or(result),
        ))
    }
    async fn run(
        &self,
        pool: &PgPool,
        runtime: DeliveryRuntime<'_>,
    ) -> Result<Option<Step>, DeliveryError> {
        let FrozenCommand {
            operation, request, ..
        } = self.frozen;

        let Some(pr) = request["pull_request"].as_u64() else {
            return Ok(Some(Step::Done(Completion::refused("no_pull_request"))));
        };
        let retained: Option<(Value, String)> = sqlx::query_as(
            "SELECT target,merge_sha FROM delivery_merge_evidence WHERE operation_id=$1",
        )
        .bind(operation)
        .fetch_optional(pool)
        .await?;
        let current = serde_json::to_value(runtime.target)?;
        let (delivery, target) = match self.runtime(runtime, retained.as_ref(), &current) {
            std::ops::ControlFlow::Continue(pair) => pair,
            std::ops::ControlFlow::Break(step) => return Ok(step),
        };
        if retained.is_none()
            && let std::ops::ControlFlow::Break(step) = self
                .confirm(pool, (delivery, target), (pr, &current))
                .await?
        {
            return Ok(step);
        }
        Ok(Some(
            delivery
                .finish_merge(
                    TaskOperation::new(operation, target, self.task),
                    pr,
                    self.heads()?,
                )
                .await,
        ))
    }

    fn runtime<'a>(
        &self,
        runtime: DeliveryRuntime<'a>,
        retained: Option<&(Value, String)>,
        current: &Value,
    ) -> std::ops::ControlFlow<Option<Step>, (&'a Delivery, &'a DeliveryTarget)> {
        use std::ops::ControlFlow::{Break, Continue};
        const MERGE: &str = "pull_request.merge";
        let request = self.frozen.request;
        let DeliveryRuntime {
            service: delivery,
            target,
        } = runtime;
        if request.get("merge_target") == Some(&Value::Null) {
            return Break(Some(without_target(MERGE)));
        }
        // Keep cleanup tied to the repository that merged, including old intents.
        let frozen = retained
            .map(|(target, _)| target)
            .or(request.get("merge_target"));
        if frozen.is_some_and(|target| target != current) {
            return Break(None);
        }
        let Some(target) = target else {
            return Break(Some(without_target(MERGE)));
        };
        let Some(delivery) = delivery else {
            return Break(if retained.is_some() {
                None
            } else {
                without_delivery(MERGE)
            });
        };
        Continue((delivery, target))
    }
    // A failed confirmation either produces a final/retry step or waits for a concurrent evidence insert.
    async fn confirm(
        &self,
        pool: &PgPool,
        (delivery, target): (&Delivery, &DeliveryTarget),
        (pr, current): (u64, &Value),
    ) -> Result<std::ops::ControlFlow<Option<Step>>, DeliveryError> {
        let request = self.frozen.request;
        let method = request["method"]
            .as_str()
            .ok_or(DeliveryError::InvalidInput("method"))?;
        let (head, pin) = self.heads()?;
        let sha = match delivery
            .merge_on_github(
                target,
                pr,
                MergeGuard {
                    method: merge_method(method),
                    expected_head: pin,
                    result_head: Some(head),
                    queue: self.frozen.queue_wait(self.now_ms),
                    check_log_bytes: request["check_log_bytes"].as_u64().unwrap_or(0),
                },
            )
            .await
        {
            super::MergeAttempt::Confirmed(sha) => sha,
            super::MergeAttempt::Unconfirmed(step) => {
                return Ok(std::ops::ControlFlow::Break(Some(step)));
            }
        };
        let number = i64::try_from(pr).map_err(|_| DeliveryError::InvalidInput("number"))?;
        let evidence = MergeEvidence {
            number,
            current,
            head,
            sha,
        };
        let inserted = self.record(pool, target, evidence).await?;
        Ok(if inserted {
            std::ops::ControlFlow::Continue(())
        } else {
            std::ops::ControlFlow::Break(None)
        })
    }
    async fn record(
        &self,
        pool: &PgPool,
        target: &DeliveryTarget,
        evidence: MergeEvidence<'_>,
    ) -> Result<bool, DeliveryError> {
        let FrozenCommand {
            operation, request, ..
        } = self.frozen;
        let MergeEvidence {
            current,
            number,
            head,
            sha,
        } = evidence;
        // Commit the merge before the separately retryable branch deletion.
        let inserted = sqlx::query(
            "INSERT INTO delivery_merge_evidence(\
             operation_id,project_id,task_id,attempt_id,result_id,pull_request,target,\
             target_ref,expected_head_sha,merge_sha,observed_at_ms) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation)
        .bind(self.project)
        .bind(self.task)
        .bind(request_uuid(request, "attempt_id"))
        .bind(request_uuid(request, "result_id"))
        .bind(number)
        .bind(current)
        .bind(target.main_ref())
        .bind(head)
        .bind(sha)
        .bind(self.now_ms)
        .execute(pool)
        .await?;
        Ok(inserted.rows_affected() != 0)
    }
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
