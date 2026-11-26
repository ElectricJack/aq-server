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
), false), EXISTS (
    SELECT 1 FROM execution_attempts a
    JOIN profiled_result_heads h USING (attempt_id)
    JOIN profiled_auto_settlements s USING (attempt_id, report_id)
    WHERE a.task_id = $1 AND $2::text IS NOT NULL
      AND s.disposition = 'rework_required' AND s.reason = 'branch_publication_rejected'
      AND s.evidence->>'decided_by' = 'owner_publication_retry'
      AND s.evidence->>'head' = $2
)
