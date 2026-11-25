SELECT r.result_head
FROM task_workspace_result_sources r
JOIN task_workspace_git_grants g USING (attempt_id, target_project_id)
JOIN execution_attempts a USING (attempt_id)
JOIN profiled_launches l USING (attempt_id)
WHERE g.home_project_id = $1 AND g.task_id = $2 AND g.target_project_id = $3
  AND g.project_revision = $4 AND g.repository_id = $5 AND g.binding_generation = $6
  AND g.commit_oid = $7 AND g.access = 'write'
  AND a.task_id = g.task_id AND NOT a.holds_task
  AND (l.launch_binding->>'assignment_generation')::bigint < $8
ORDER BY (l.launch_binding->>'assignment_generation')::bigint DESC, r.recorded_at_ms DESC
LIMIT 1
