SELECT COALESCE((
    SELECT (decision = 'returned' AND COALESCE(update_from_main, false))
        OR (decision = 'held' AND COALESCE(reason = 'returns_exhausted', false) AND EXISTS (
            SELECT 1 FROM playbook_host_operations o
            WHERE o.subject_id = d.task_id::text
              AND o.command_name IN ('pull_request.await_checks', 'pull_request.merge')
              AND o.status = 'conflict'
              AND o.request#>>'{result_ready,result_id}' = d.result_id::text
              AND COALESCE(o.request->>'result_head', o.request->>'expected_head_sha') = $2
              AND COALESCE(o.request->'check_target', o.request->'merge_target')->'git' @> $3::jsonb
        ))
    FROM delivery_decisions d WHERE task_id = $1 ORDER BY decided_at_ms DESC, result_id LIMIT 1
), false) OR EXISTS (
    SELECT 1 FROM owner_task_retry_intents i
    JOIN profiled_result_heads h ON h.attempt_id = i.predecessor
    JOIN profiled_auto_settlements s USING (attempt_id, report_id)
    JOIN delivery_pushed_results p USING (attempt_id, report_id)
    JOIN delivery_git_grants g USING (attempt_id)
    JOIN profiled_launches l USING (attempt_id)
    WHERE i.task_id = $1 AND i.project_id = $4 AND i.project_revision = $5
      AND i.released_at IS NOT NULL
      AND p.head = $2 AND g.repository_id = ($3::jsonb->>'repository_id')::uuid AND p.task_ref = $6 AND g.task_ref = $6
      AND l.execution_spec#>>'{source,repository_binding_generation}' = $3::jsonb->>'binding_generation'
      AND s.disposition = 'rework_required' AND s.reason = 'branch_publication_rejected'
      AND s.evidence->>'decided_by' = 'owner_publication_retry'
      AND s.evidence->>'owner_operation_id' = i.operation_id::text
      AND s.evidence->>'head' = p.head
)
