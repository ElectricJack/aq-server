SELECT a.task_id,
       home.project_id AS home_project_id,
       (l.execution_spec #>> '{source,repository_id}')::uuid AS home_repository,
       (l.launch_binding->>'assignment_generation')::bigint AS generation
FROM execution_attempts a
JOIN profiled_launches l USING (attempt_id)
JOIN profiled_authority_hot hot USING (attempt_id)
JOIN project_execution_bindings home
    ON home.project_id::text = l.launch_manifest #>> '{bindings,project_id}'
WHERE a.attempt_id = $1 AND a.node_id = $2 AND a.holds_task
  AND a.kind = 'profiled_v1'
  AND home.binding->>'enabled' = 'true'
  AND hot.state->>'revoked' = 'false'
  AND hot.state->>'cancellation_id' IS NULL
  AND NOT EXISTS (SELECT 1 FROM profiled_result_reports r WHERE r.attempt_id = a.attempt_id)
FOR SHARE OF a, hot, home
