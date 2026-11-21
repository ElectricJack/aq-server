//! Host catalog 2.3.0 (Epic 0, spec 0016 sections 6.1 and 6.2), the server's
//! side in PostgreSQL: which results wait for a playbook's decision, the facts
//! of a task's latest result, and the emission state of
//! `aq.work.result_ready`. The rules are in [`crate::host10`]; this module
//! only reads and writes rows. Compiled in only with the cargo feature
//! `delivery_playbook`.
//!
//! Nothing here decides anything about a result. `delivery_decisions`,
//! `delivery_main_heads` and `delivery_pull_requests` are written by the
//! commands and the main sync that report into them; this module only reads
//! them.
use crate::{
    Result,
    host9::WorkFacts,
    host10::{
        self, DELIVERY_BINDING, PullRequestFacts, ResultEmission, ResultFacts, ResultReady,
        branch_of, facts_digest, result_ready_due,
    },
    stable_id,
};
use aq_playbook_host::model::{EventContract, EventEnvelope};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// A task whose current attempt has stopped and recorded a result, and whose
/// claim is still held: the result nothing has settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub task_id: Uuid,
    pub workspace_id: Uuid,
    pub project_id: Uuid,
    pub attempt_id: Uuid,
    pub result_id: Uuid,
}

/// The latest result of a task, as the server recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Latest {
    pub workspace_id: Uuid,
    pub project_id: Uuid,
    pub attempt_id: Uuid,
    pub result_id: Uuid,
    /// The outcome the agent reported (`success`, `failed`, ...).
    pub outcome: String,
    /// The retained source differs from the lineage's base.
    pub changed: bool,
    /// The commit the result's source was retained at, or `""`.
    pub head: String,
    /// The commit the lineage started from, or `""`.
    pub base: String,
}

/// What the host reads of the server's execution records. A port, so that
/// the emission and `work.read` can be tested without recording a result.
#[async_trait::async_trait]
pub trait DeliveryPort: Send + Sync {
    /// Results waiting for a decision, at most `limit`, the longest
    /// unannounced first. `project` limits them to one project. Results with a
    /// decision on them are not returned.
    async fn pending(&self, project: Option<Uuid>, limit: u32) -> Result<Vec<Pending>>;
    /// The latest result of `task`, if it has one.
    async fn latest(&self, task: Uuid) -> Result<Option<Latest>>;
    /// Read the same facts inside the caller's delivery transaction.
    async fn latest_in(&self, _db: &mut PgConnection, task: Uuid) -> Result<Option<Latest>> {
        self.latest(task).await
    }
    /// Lock the event's attempt and verify its current result before an effect.
    /// Fixture ports use the equivalent identity check against their own store.
    async fn lock_current(&self, _db: &mut PgConnection, event: &ResultReady) -> Result<bool> {
        Ok(self.latest(event.task_id).await?.is_some_and(|latest| {
            latest.project_id == event.project_id
                && latest.attempt_id == event.attempt_id
                && latest.result_id == event.result_id
        }))
    }

    /// Whether the server runs tasks of `project` (it has an execution binding).
    async fn bound(&self, project: Uuid) -> Result<bool>;
    /// Whether `result` is a result of `task`, its latest or an earlier one.
    async fn has_result(&self, task: Uuid, result: Uuid) -> Result<bool>;
}

/// Production port: the server's own database.
pub struct PgDelivery(pub PgPool);

#[async_trait::async_trait]
impl DeliveryPort for PgDelivery {
    async fn pending(&self, project: Option<Uuid>, limit: u32) -> Result<Vec<Pending>> {
        let rows = sqlx::query(
            "SELECT a.task_id,(l.execution_spec#>>'{task,workspace_id}')::uuid AS workspace_id,\
             (l.execution_spec#>>'{lifecycle,project_id}')::uuid AS project_id,a.attempt_id,h.report_id \
             FROM execution_attempts a \
             JOIN delivery_git_grants g ON g.attempt_id=a.attempt_id \
             JOIN profiled_authority_hot ah ON ah.attempt_id=a.attempt_id \
             JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id \
             JOIN profiled_launches l ON l.attempt_id=a.attempt_id \
             LEFT JOIN delivery_tasks t ON t.task_id=a.task_id \
             WHERE a.kind='profiled_v1' AND a.holds_task AND ah.state#>>'{stop,kind}'='stopped' \
             AND ($1::uuid IS NULL OR (l.execution_spec#>>'{lifecycle,project_id}')::uuid=$1) \
             AND NOT EXISTS(SELECT 1 FROM delivery_decisions d WHERE d.result_id=h.report_id) \
             ORDER BY GREATEST(COALESCE(t.emitted_at_ms,0),COALESCE(t.checked_at_ms,0)),h.report_id \
             LIMIT $2",
        )
        .bind(project)
        .bind(i64::from(limit))
        .fetch_all(&self.0)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Pending {
                task_id: r.get("task_id"),
                workspace_id: r.get("workspace_id"),
                project_id: r.get("project_id"),
                attempt_id: r.get("attempt_id"),
                result_id: r.get("report_id"),
            })
            .collect())
    }

    async fn latest(&self, task: Uuid) -> Result<Option<Latest>> {
        let mut conn = self.0.acquire().await?;
        self.latest_in(&mut conn, task).await
    }

    async fn latest_in(&self, conn: &mut PgConnection, task: Uuid) -> Result<Option<Latest>> {
        let row = sqlx::query(
            "SELECT r.report_id,r.attempt_id,(l.execution_spec#>>'{task,workspace_id}')::uuid AS workspace_id,\
             (l.execution_spec#>>'{lifecycle,project_id}')::uuid AS project_id,\
             convert_from(r.report_bytes,'UTF8')::jsonb AS report,g.attempt_id IS NOT NULL AS enrolled \
             FROM execution_attempts a \
             JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id \
             JOIN profiled_result_reports r ON r.report_id=h.report_id \
             JOIN profiled_launches l ON l.attempt_id=a.attempt_id \
             LEFT JOIN delivery_git_grants g ON g.attempt_id=a.attempt_id \
             WHERE a.task_id=$1 \
             ORDER BY (convert_from(r.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint DESC,r.report_id DESC \
             LIMIT 1",
        )
        .bind(task)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row.filter(|r| r.get::<bool, _>("enrolled")) else {
            return Ok(None);
        };
        let (attempt, report): (Uuid, serde_json::Value) =
            (row.get("attempt_id"), row.get("report"));
        // Spec 0016 §5.1: the pushed task ref, confirmed at its head when the
        // report was recorded, from the commit the attempt was offered.
        let start: Option<String> = sqlx::query_scalar(
            "SELECT execution_spec#>>'{source,resolved_object,hex}' FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        // A push whose lease no longer held names no head: its outcome is
        // `push_conflict`, so the policy can send the task back.
        let pushed: Option<(Option<String>,)> =
            sqlx::query_as("SELECT head FROM delivery_pushed_results WHERE report_id=$1")
                .bind(row.get::<Uuid, _>("report_id"))
                .fetch_optional(&mut *conn)
                .await?;
        let conflict = matches!(pushed, Some((None,)));
        let head = pushed.and_then(|(head,)| head).unwrap_or_default();
        let start = if head.is_empty() {
            String::new()
        } else {
            start.unwrap_or_default()
        };
        // The lineage's original base; past the handoff bound it is unknown,
        // and a result with a source then counts as changed.
        let base = match start.is_empty() {
            true => None,
            false => aq_machine_control::lineage_base(conn, task, &start)
                .await
                .map_err(|_| crate::Error::Unknown)?,
        };
        Ok(Some(Latest {
            workspace_id: row.get("workspace_id"),
            project_id: row.get("project_id"),
            attempt_id: attempt,
            result_id: row.get("report_id"),
            outcome: match conflict {
                true => "push_conflict".to_owned(),
                false => report["outcome"].as_str().unwrap_or("").to_owned(),
            },
            changed: !head.is_empty() && base.as_deref() != Some(head.as_str()),
            head,
            base: base.unwrap_or_default(),
        }))
    }

    async fn lock_current(&self, db: &mut PgConnection, event: &ResultReady) -> Result<bool> {
        // The result writer holds this attempt lock before replacing its report head.
        let held: Option<bool> = sqlx::query_scalar(
            "SELECT a.holds_task FROM execution_attempts a JOIN profiled_launches l USING (attempt_id)
             WHERE a.attempt_id=$1 AND a.task_id=$2
             AND l.execution_spec#>>'{lifecycle,project_id}'=$3 FOR UPDATE OF a",
        )
        .bind(event.attempt_id)
        .bind(event.task_id)
        .bind(event.project_id.to_string())
        .fetch_optional(&mut *db)
        .await?;
        if held != Some(true) {
            return Ok(false);
        }
        let current: Option<Uuid> = sqlx::query_scalar(
            "SELECT report_id FROM profiled_result_heads WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(event.attempt_id)
        .fetch_optional(db)
        .await?;
        Ok(current == Some(event.result_id))
    }

    async fn bound(&self, project: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM project_execution_bindings WHERE project_id=$1)",
        )
        .bind(project)
        .fetch_one(&self.0)
        .await?)
    }

    async fn has_result(&self, task: Uuid, result: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM profiled_result_reports r \
             JOIN execution_attempts a ON a.attempt_id=r.attempt_id \
             JOIN delivery_git_grants g ON g.attempt_id=a.attempt_id WHERE r.report_id=$2 AND a.task_id=$1)",
        )
        .bind(task)
        .bind(result)
        .fetch_one(&self.0)
        .await?)
    }
}

/// The facts of a task's latest result for `work.read`. `latest` is `None`
/// when the task has no result: its result facts are then empty, while the
/// host-counted `returns`, the commit main points at and a pull request it
/// may have are still told.
pub async fn result_facts(
    db: &mut PgConnection,
    task: Uuid,
    project: Uuid,
    latest: Option<&Latest>,
) -> Result<ResultFacts> {
    let returns: Option<i32> =
        sqlx::query_scalar("SELECT returns FROM delivery_tasks WHERE task_id=$1")
            .bind(task)
            .fetch_optional(&mut *db)
            .await?;
    let main_head: Option<String> =
        sqlx::query_scalar("SELECT head FROM delivery_main_heads WHERE project_id=$1")
            .bind(project)
            .fetch_optional(&mut *db)
            .await?;
    let pull_request = sqlx::query("SELECT number,url,state,mergeable,checks_state,checks_failed FROM delivery_pull_requests WHERE task_id=$1")
        .bind(task)
        .fetch_optional(&mut *db)
        .await?
        .map(|r| PullRequestFacts {
            number: u64::try_from(r.get::<i64, _>("number")).unwrap_or(0),
            url: r.get("url"),
            state: r.get("state"),
            mergeable: r.get("mergeable"),
            checks_state: r.get("checks_state"),
            checks_failed: serde_json::from_value(r.get("checks_failed")).unwrap_or_default(),
        });
    let mut facts = ResultFacts {
        returns: u32::try_from(returns.unwrap_or(0)).unwrap_or(0),
        main_head: main_head.unwrap_or_default(),
        pull_request,
        ..Default::default()
    };
    if let Some(latest) = latest {
        facts.outcome = latest.outcome.clone();
        facts.changed = latest.changed;
        facts.branch = branch_of(task);
        facts.head = latest.head.clone();
        facts.base = latest.base.clone();
    }
    Ok(facts)
}

/// Where `task` is and whether the server runs it: its latest result, its
/// workspace and project (those of the result, else `fallback`) and whether
/// the project is bound.
pub async fn locate(
    port: &dyn DeliveryPort,
    task: Uuid,
    fallback: (Uuid, Uuid),
) -> Result<(Option<Latest>, (Uuid, Uuid), bool)> {
    let latest = port.latest(task).await?;
    let at = latest
        .as_ref()
        .map_or(fallback, |l| (l.workspace_id, l.project_id));
    Ok((latest, at, port.bound(at.1).await?))
}

/// The `observed` output of `work.read` under catalog 2.3.0 for the graph
/// node `node` at `at` (`task_facts`: whether a playbook of this namespace
/// filed it, its charged failures and its ended attempts).
pub async fn observed(
    db: &mut PgConnection,
    task: Uuid,
    at: (Uuid, Uuid),
    latest: Option<&Latest>,
    node: &Value,
    task_facts: (bool, u32, u32),
) -> Result<Value> {
    let version = node["version"]
        .as_i64()
        .filter(|v| *v >= 1)
        .ok_or(crate::Error::Unknown)?;
    let mut work = WorkFacts::of(node, task_facts.0, task_facts.1, task_facts.2);
    let mut facts = result_facts(db, task, at.1, latest).await?;
    if let Some(result) = latest
        && crate::delivery_review::designated(node)
    {
        work.kind = "review-only".into();
        facts.outcome = crate::delivery_review::outcome(db, node, task, version, result)
            .await?
            .to_owned();
    }
    Ok(host10::read_output(task, version, &work, &facts))
}

/// The results to announce now: none unless an artifact is active on the
/// delivery binding of the namespace, else the pending results of `project`
/// (all projects when `None`) that are due ([`due`]).
pub async fn plan(
    db: &mut PgConnection,
    namespace: (Uuid, i64),
    port: &dyn DeliveryPort,
    project: Option<Uuid>,
    limit: u32,
    now_ms: i64,
    (repeat_ms, run_bound_ms): (i64, i64),
) -> Result<Vec<Due>> {
    if !active(db, namespace).await? {
        return Ok(vec![]);
    }
    let pending = port.pending(project, limit).await?;
    due(db, namespace, pending, now_ms, repeat_ms, run_bound_ms).await
}

/// The latest result of `attempt` with no playbook decision on it and no
/// `work.complete` or `work.return` settling it (the task read says it waits).
pub async fn undecided(db: &mut PgConnection, attempt: Uuid) -> Result<Option<Uuid>> {
    Ok(result_wait(db, attempt)
        .await?
        .and_then(|(id, hold)| hold.is_none().then_some(id)))
}

/// The claim's current undecided or held result, with its recorded hold reason.
pub async fn result_wait(
    db: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<(Uuid, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT h.report_id,CASE WHEN d.decision='held' THEN COALESCE(d.reason,'delivery_hold') END
         FROM profiled_result_heads h JOIN delivery_git_grants g USING(attempt_id)
         LEFT JOIN delivery_decisions d ON d.result_id=h.report_id
         WHERE h.attempt_id=$1 AND (d.decision IS NULL OR d.decision='held')
         AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.report_id=h.report_id \
         AND a.disposition IN ('prepared','possible','confirmed','rework_required'))",
    )
    .bind(attempt)
    .fetch_optional(db)
    .await?)
}

/// The latest refused publication for the claimed task, result and retained head.
/// Older hosts recorded only the catalog reason. A later success supersedes all refusals.
pub async fn publication_refusal(db: &mut PgConnection, result: Uuid) -> Result<Option<String>> {
    let refusal: Option<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT o.operation_id,p.head,COALESCE(o.request->>'answer_note'='remote_rejected',false)
         FROM delivery_pushed_results p JOIN profiled_result_heads h USING(report_id,attempt_id)
         JOIN execution_attempts a USING(attempt_id)
         JOIN playbook_host_operations o ON o.subject_id=a.task_id::text AND o.command_name='branch.publish'
           AND o.request#>>'{result_ready,result_id}'=p.report_id::text
           AND o.request#>>'{result_ready,attempt_id}'=p.attempt_id::text AND o.request->>'result_head'=p.head
         WHERE p.report_id=$1 AND a.holds_task AND o.status='refused' AND o.output->>'reason'='unreachable'
         AND NOT EXISTS(SELECT 1 FROM playbook_host_operations ok WHERE ok.subject_id=o.subject_id
             AND ok.command_name='branch.publish' AND ok.status='published'
             AND ok.request#>>'{result_ready,result_id}'=p.report_id::text
             AND ok.request#>>'{result_ready,attempt_id}'=p.attempt_id::text AND ok.output->>'head'=p.head)
         ORDER BY o.completed_at_ms DESC,o.operation_id LIMIT 1",
    ).bind(result).fetch_optional(db).await?;
    Ok(refusal.map(|(id, head, rejected)| {
        let cause = if rejected {
            "AQ Git recorded a rejection from the remote"
        } else {
            "the delivery host recorded an unreachable response"
        };
        format!("Publication of commit {head} failed ({cause}). AQ Git operation {id}.")
    }))
}

/// A check operation still running for this exact result, attempt and published head.
pub async fn checking_pr(db: &mut PgConnection, result: Uuid) -> Result<Option<String>> {
    let check: Option<(String, String)> = sqlx::query_as(
        "SELECT o.request->>'pull_request',p.head FROM delivery_pushed_results p
         JOIN profiled_result_heads h ON h.report_id=p.report_id JOIN execution_attempts a ON a.attempt_id=h.attempt_id
         JOIN playbook_host_operations o ON o.subject_id=a.task_id::text
         WHERE p.report_id=$1 AND a.holds_task AND o.command_name='pull_request.await_checks' AND o.status='running'
           AND o.request#>>'{result_ready,result_id}'=p.report_id::text
           AND o.request#>>'{result_ready,attempt_id}'=a.attempt_id::text
           AND o.request->>'result_head'=p.head AND jsonb_typeof(o.request->'pull_request')='number'
         ORDER BY o.created_at_ms DESC,o.operation_id LIMIT 1",
    )
    .bind(result)
    .fetch_optional(db)
    .await?;
    Ok(check.map(|(number, head)| {
        format!("AQ is waiting for required checks on PR #{number} at commit {head}.")
    }))
}

/// The current claimed report's actual outcome and recorded return, for a
/// project supervisor inspecting a budget hold. Only bounded status fields
/// pass through; the report body stays on its own route.
pub async fn returned_result_evidence(
    db: &mut PgConnection,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Result<Option<String>> {
    let record = sqlx::query(
        "SELECT h.report_id,convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome' AS outcome,s.disposition,p.head,
                CASE WHEN d.reason='checks_failed' AND char_length(d.detail) BETWEEN 1 AND 512
                          AND d.detail !~ '[[:cntrl:]]' THEN d.detail END AS check_detail
         FROM execution_attempts a JOIN profiled_launches l USING(attempt_id)
         JOIN profiled_result_heads h USING(attempt_id)
         JOIN profiled_result_reports r USING(attempt_id,report_id)
         JOIN delivery_decisions d ON d.result_id=h.report_id AND d.task_id=a.task_id AND d.decision='returned'
         LEFT JOIN profiled_auto_settlements s ON s.report_id=h.report_id AND s.attempt_id=a.attempt_id
         LEFT JOIN delivery_pushed_results p ON p.report_id=h.report_id AND p.attempt_id=a.attempt_id
         WHERE a.holds_task AND a.task_id=$2 AND a.attempt_id=$3 AND l.execution_spec#>>'{lifecycle,project_id}'=$1",
    )
    .bind(project.to_string())
    .bind(task)
    .bind(attempt)
    .fetch_optional(db)
    .await?;
    Ok(record.map(|row| {
        let result: Uuid = row.get("report_id");
        let outcome: String = row.get("outcome");
        let mut text =
            format!("The delivery playbook returned result {result}. Worker outcome: {outcome}.");
        if let Some(disposition) = row.get::<Option<String>, _>("disposition") {
            text.push_str(&format!(" Recorded settlement: {disposition}."));
        }
        if let Some(head) = row.get::<Option<String>, _>("head") {
            text.push_str(&format!(" Retained commit: {head}."));
        }
        if let Some(detail) = row.get::<Option<String>, _>("check_detail") {
            text.push_str(&format!(" Recorded failed check detail: {detail:?}. Inspect the failed workflow step before requesting source changes."));
        }
        text
    }))
}

/// Last final PR failure observed by AQ on this result's published head.
pub async fn held_pr_evidence(db: &mut PgConnection, result: Uuid) -> Result<Option<String>> {
    let observed: Option<(String, String, String, Uuid, bool)> = sqlx::query_as(
        "SELECT o.command_name,o.status,p.head,a.task_id,COALESCE(o.request->>'answer_note'='behind_base',false) FROM delivery_pushed_results p
         JOIN profiled_result_heads h ON h.report_id=p.report_id JOIN execution_attempts a ON a.attempt_id=h.attempt_id
         JOIN playbook_host_operations o ON o.subject_id=a.task_id::text
         WHERE p.report_id=$1 AND o.request#>>'{result_ready,result_id}'=p.report_id::text
           AND o.request#>>'{result_ready,attempt_id}'=a.attempt_id::text
           AND COALESCE(o.request->>'result_head',o.request->>'expected_head_sha')=p.head
           AND ((o.command_name='pull_request.await_checks' AND o.status IN ('conflict','failed'))
             OR (o.command_name='pull_request.merge' AND o.status='conflict'))
         ORDER BY o.completed_at_ms DESC NULLS LAST,o.operation_id LIMIT 1",
    )
    .bind(result)
    .fetch_optional(&mut *db)
    .await?;
    let Some((command, status, head, task, behind)) = observed else {
        return Ok(None);
    };
    let mut evidence = format!("AQ recorded {status} from {command} on published head {head}.");
    if behind {
        evidence.push_str(" GitHub reported that this head is behind its base branch.");
    }
    if aq_delivery::git_grant::held_report(&mut *db, task).await? == Some(result) {
        evidence.push_str(
            " AQ verified that this held result qualifies for repair through task.retry.",
        );
        if status == "conflict" {
            evidence.push_str(" A replacement can integrate current main if the retained task ref and repository binding still match.");
        }
    }
    Ok(Some(evidence))
}

/// Whether an artifact is active on the delivery binding of the namespace:
/// the event is sent only then (and the task read says so).
pub async fn active(db: &mut PgConnection, namespace: (Uuid, i64)) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_engine_bindings WHERE namespace_id=$1 AND generation=$2 AND binding=$3)",
    )
    .bind(namespace.0)
    .bind(namespace.1)
    .bind(DELIVERY_BINDING)
    .fetch_one(db)
    .await?)
}

/// A pending result that is due an `aq.work.result_ready`, with the digest of
/// the facts it is sent under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub pending: Pending,
    pub facts: String,
}
impl Due {
    /// The ids the event carries, for the task at `version`.
    pub fn ready(&self, version: i64) -> ResultReady {
        ResultReady {
            task_id: self.pending.task_id,
            project_id: self.pending.project_id,
            attempt_id: self.pending.attempt_id,
            result_id: self.pending.result_id,
            version,
        }
    }
    /// The `aq.work.result_ready` event for the task at `version`, with its id.
    pub fn event(
        &self,
        version: i64,
        namespace: (Uuid, i64),
        now_ms: i64,
        contract: &EventContract,
    ) -> Result<(Uuid, EventEnvelope)> {
        // The facts are in the key: a send repeated at once because they moved
        // may fall in the millisecond of the send before it.
        let key = format!(
            "{}/{}/{now_ms}/{}",
            self.pending.task_id, self.pending.result_id, self.facts
        );
        let id = stable_id("aq.work.result_ready", namespace.0, namespace.1, &key)?;
        let event = host10::result_ready_event(id, &self.ready(version), now_ms, contract);
        Ok((id, event.map_err(|_| crate::Error::Invalid)?))
    }
}

/// The pending results to send now ([`result_ready_due`]); the others are
/// marked as looked at, so that they go behind those not looked at yet.
/// `run_bound_ms` is how long a run that read the task still holds back a
/// repeat, whether or not its end is on record.
async fn due(
    db: &mut PgConnection,
    namespace: (Uuid, i64),
    pending: Vec<Pending>,
    now_ms: i64,
    repeat_ms: i64,
    run_bound_ms: i64,
) -> Result<Vec<Due>> {
    let results: Vec<Uuid> = pending.iter().map(|p| p.result_id).collect();
    let decided: Vec<Uuid> =
        sqlx::query_scalar("SELECT result_id FROM delivery_decisions WHERE result_id=ANY($1)")
            .bind(&results)
            .fetch_all(&mut *db)
            .await?;
    let mut out = vec![];
    for p in pending
        .into_iter()
        .filter(|p| !decided.contains(&p.result_id))
    {
        let row = sqlx::query("SELECT emitted_result_id,emitted_at_ms,emitted_facts FROM delivery_tasks WHERE task_id=$1 FOR UPDATE")
            .bind(p.task_id)
            .fetch_optional(&mut *db)
            .await?;
        let mut emission = ResultEmission::default();
        if let Some(row) = &row {
            emission.emitted_result = row.get("emitted_result_id");
            emission.emitted_at_ms = row.get("emitted_at_ms");
            emission.emitted_facts = row.get("emitted_facts");
        }
        if emission.emitted_result == Some(p.result_id) {
            emission.run_unfinished = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM playbook_effects e WHERE e.namespace_id=$1 AND e.generation=$2 \
                 AND e.command_name='work.read' AND e.receipt->'subject'->>'id'=$3 \
                 AND e.created_at_ms>=$4 AND e.created_at_ms>$5 AND NOT playbook_run_finished($1,$2,e.run_id))",
            )
            .bind(namespace.0)
            .bind(namespace.1)
            .bind(p.task_id.to_string())
            .bind(emission.emitted_at_ms)
            .bind(now_ms - run_bound_ms)
            .fetch_one(&mut *db)
            .await?;
        }
        let moving = result_facts(db, p.task_id, p.project_id, None).await?;
        let facts = facts_digest(&moving.main_head, moving.pull_request.as_ref());
        if result_ready_due(&emission, p.result_id, &facts, now_ms, repeat_ms) {
            out.push(Due { pending: p, facts });
        } else {
            sqlx::query("UPDATE delivery_tasks SET checked_at_ms=$2 WHERE task_id=$1")
                .bind(p.task_id)
                .bind(now_ms)
                .execute(&mut *db)
                .await?;
        }
    }
    Ok(out)
}

/// Records that `aq.work.result_ready` was sent for `due` at `now_ms`.
pub async fn sent(db: &mut PgConnection, due: &Due, now_ms: i64) -> Result<()> {
    sqlx::query("INSERT INTO delivery_tasks(task_id,emitted_result_id,emitted_at_ms,emitted_facts) VALUES($1,$2,$3,$4) ON CONFLICT(task_id) DO UPDATE SET emitted_result_id=$2,emitted_at_ms=$3,emitted_facts=$4,checked_at_ms=0")
        .bind(due.pending.task_id)
        .bind(due.pending.result_id)
        .bind(now_ms)
        .bind(&due.facts)
        .execute(db)
        .await?;
    Ok(())
}
