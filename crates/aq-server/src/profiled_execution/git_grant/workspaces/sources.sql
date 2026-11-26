SELECT s.home_project_id, s.task_id, s.target_project_id, s.project_revision,
       s.repository_id, s.binding_generation, s.commit_oid, s.access
FROM task_workspace_sources s
JOIN project_execution_bindings t ON t.project_id = s.target_project_id
JOIN task_workspace_offer_fences f ON f.attempt_id = $3
WHERE s.home_project_id = $1 AND s.task_id = $2 AND s.state = 'approved'
  AND s.target_project_id <> s.home_project_id
  AND s.requested_attempt_id IS DISTINCT FROM $3
  AND t.revision = s.project_revision AND t.binding->>'enabled' = 'true'
  AND t.binding->>'repository_id' = s.repository_id::text
  AND t.binding->>'repository_binding_generation' = s.binding_generation::text
  AND f.scope->'sources' @> jsonb_build_array(jsonb_build_object(
      'target_project_id', s.target_project_id, 'project_revision', s.project_revision,
      'repository_id', s.repository_id, 'binding_generation', s.binding_generation,
      'commit_oid', s.commit_oid, 'access', s.access))
ORDER BY s.target_project_id LIMIT $4
FOR SHARE OF s, t
