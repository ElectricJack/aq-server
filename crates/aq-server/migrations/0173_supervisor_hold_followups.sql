-- A reminder is a scheduler-owned follow-up for one current hold. No
-- delivery decision, retry authorization or task claim is written here.
CREATE TABLE supervisor_hold_followups (
    project_id UUID NOT NULL,
    identity_digest TEXT NOT NULL CHECK (identity_digest ~ '^sha256:[0-9a-f]{64}$'),
    since_ms BIGINT NOT NULL CHECK (since_ms >= 0),
    task_id UUID NOT NULL,
    attempt_id UUID NOT NULL,
    next_check_at_ms BIGINT NOT NULL CHECK (next_check_at_ms > 0),
    initial_message_id UUID,
    last_message_id UUID,
    reminders_sent INTEGER NOT NULL DEFAULT 0 CHECK (reminders_sent BETWEEN 0 AND 3),
    last_reply_id UUID,
    owner_attention BOOLEAN NOT NULL DEFAULT FALSE,
    closed_at_ms BIGINT CHECK (closed_at_ms > 0),
    PRIMARY KEY (project_id, identity_digest, since_ms),
    CHECK (reminders_sent = 0 OR (initial_message_id IS NOT NULL AND last_message_id IS NOT NULL)),
    CHECK (NOT owner_attention OR reminders_sent = 3)
);
CREATE INDEX supervisor_hold_followups_due ON supervisor_hold_followups(next_check_at_ms,project_id,identity_digest) WHERE closed_at_ms IS NULL;
CREATE INDEX supervisor_hold_followups_task ON supervisor_hold_followups(project_id,task_id);

-- The sweep and Work use the same source of truth for which hold owns a follow-up.
CREATE VIEW supervisor_hold_waits AS
WITH waits AS (
 SELECT h.project_id, h.task_id, h.attempt_id, h.comment_id AS identity, h.reason,
        (extract(epoch FROM h.held_at)*1000)::bigint AS since_ms
 FROM task_holds_current h
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, s.report_id,
        CASE WHEN hot.state#>>'{effects,kind}'<>'quiescent' THEN 'replacement_effects_unreconciled'
             ELSE 'replacement_source_retention_unknown' END, s.updated_at_ms
 FROM profiled_auto_settlements s JOIN execution_attempts a USING(attempt_id)
 JOIN profiled_result_heads current ON current.attempt_id=a.attempt_id AND current.report_id=s.report_id
 JOIN profiled_authority_hot hot ON hot.attempt_id=a.attempt_id
 JOIN profiled_launches l ON l.attempt_id=a.attempt_id
 WHERE a.holds_task AND s.disposition='rework_required' AND hot.state#>>'{stop,kind}'='stopped'
 AND (hot.state#>>'{effects,kind}'<>'quiescent' OR hot.state#>>'{retention,kind}'<>'retained')
 AND NOT EXISTS(SELECT 1 FROM task_holds_current h WHERE h.task_id=a.task_id)
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, a.attempt_id,
        'stopped_without_result', (convert_from(s.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint
 FROM execution_attempts a JOIN profiled_stopped_receipts s USING(attempt_id)
 JOIN profiled_launches l USING(attempt_id)
 WHERE a.holds_task AND NOT EXISTS(SELECT 1 FROM profiled_result_heads h WHERE h.attempt_id=a.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM task_holds_current h WHERE h.task_id=a.task_id)
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, a.attempt_id,
        'launch_closure_pending', c.recorded_at_ms
 FROM execution_attempts a JOIN profiled_launch_closures c USING(attempt_id)
 JOIN profiled_launches l USING(attempt_id)
 JOIN profiled_authority_hot hot ON hot.attempt_id=a.attempt_id
 WHERE a.holds_task AND hot.state#>>'{stop,kind}' IN ('unknown','not_started')
 AND NOT EXISTS(SELECT 1 FROM task_holds_current h WHERE h.task_id=a.task_id)
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid,a.task_id,a.attempt_id,h.report_id,
        'branch_publication_refused',min(o.completed_at_ms)
 FROM execution_attempts a JOIN profiled_result_heads h USING(attempt_id)
 JOIN profiled_launches l USING(attempt_id)
 JOIN delivery_pushed_results p ON p.report_id=h.report_id AND p.attempt_id=a.attempt_id
 JOIN playbook_host_operations o ON o.subject_id=a.task_id::text AND o.command_name='branch.publish'
     AND o.request#>>'{result_ready,attempt_id}'=a.attempt_id::text
     AND o.request#>>'{result_ready,result_id}'=h.report_id::text AND o.request->>'result_head'=p.head
 WHERE a.holds_task AND NOT EXISTS(SELECT 1 FROM delivery_decisions d WHERE d.result_id=h.report_id)
 AND NOT EXISTS(SELECT 1 FROM task_holds_current current WHERE current.task_id=a.task_id)
 GROUP BY l.execution_spec#>>'{lifecycle,project_id}',a.task_id,a.attempt_id,h.report_id
 HAVING count(*) FILTER(WHERE o.status='refused' AND o.output->>'reason'='unreachable')>=2
    AND bool_and(o.status='running' OR (o.status='refused' AND o.output->>'reason'='unreachable'))
), keyed AS (
 SELECT w.*, 'sha256:'||encode(sha256(convert_to(
        CASE reason WHEN 'branch_publication_refused' THEN 'aq.scheduler.publish/'
                    WHEN 'launch_closure_pending' THEN 'aq.scheduler.launch/'
                    ELSE 'aq.scheduler.hold/' END||identity::text,'UTF8')),'hex') AS key,
        false AS delivery
 FROM waits w WHERE since_ms IS NOT NULL
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, h.report_id,
        COALESCE(d.reason,'delivery_hold'), d.decided_at_ms,
        'sha256:'||encode(sha256(convert_to('['||to_json('aq.delivery.supervisor_notify'::text)::text||','||
             to_json(h.report_id::text||':'||COALESCE(d.reason,'delivery_hold'))::text||']','UTF8')),'hex'),
        true
 FROM delivery_decisions d JOIN profiled_result_heads h ON h.report_id=d.result_id
 JOIN execution_attempts a ON a.attempt_id=h.attempt_id AND a.task_id=d.task_id
 JOIN profiled_launches l ON l.attempt_id=a.attempt_id
 WHERE d.decision='held' AND a.holds_task
)

SELECT DISTINCT ON (k.project_id,k.task_id)
       k.project_id,k.task_id,k.attempt_id,k.identity,k.key,k.reason,k.since_ms,
       h.report_id,l.execution_spec#>>'{task,version}' AS version,p.url,k.delivery,
       COALESCE(l.execution_spec#>>'{task,workspace_id}',a.state#>>'{offer,manifest,bindings,graph_id}')::uuid AS workspace_id
FROM keyed k JOIN execution_attempts a USING(attempt_id)
LEFT JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id
LEFT JOIN profiled_launches l ON l.attempt_id=a.attempt_id
LEFT JOIN delivery_pull_requests p ON p.task_id=k.task_id
WHERE (NOT k.delivery OR (a.holds_task AND h.report_id=k.identity))
  AND NOT EXISTS (SELECT 1 FROM profiled_stopped_dispositions d WHERE d.task_id=k.task_id)
  AND NOT EXISTS (SELECT 1 FROM profiled_task_completions c WHERE c.task_id=k.task_id AND c.state='confirmed')
ORDER BY k.project_id,k.task_id,k.delivery ASC,k.since_ms DESC,k.identity;

UPDATE meta SET value='173' WHERE key='state_format_version';
