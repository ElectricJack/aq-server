SELECT g.*
FROM task_workspace_git_grants g
JOIN task_workspace_sources s USING (home_project_id, task_id, target_project_id)
JOIN project_execution_bindings t ON t.project_id = s.target_project_id
JOIN task_workspace_offer_fences f ON f.attempt_id = g.attempt_id
WHERE g.attempt_id = $1 AND g.target_project_id = $2 AND g.home_project_id = $3 AND g.task_id = $4
  AND s.state = 'approved' AND s.requested_attempt_id IS DISTINCT FROM g.attempt_id
  AND (g.project_revision, g.repository_id, g.binding_generation, g.commit_oid, g.access)
      = (s.project_revision, s.repository_id, s.binding_generation, s.commit_oid, s.access)
  AND t.revision = s.project_revision AND t.binding->>'enabled' = 'true'
  AND t.binding->>'repository_id' = s.repository_id::text
  AND t.binding->>'repository_binding_generation' = s.binding_generation::text
  AND f.scope->'sources' @> jsonb_build_array(jsonb_build_object(
      'target_project_id', s.target_project_id, 'project_revision', s.project_revision,
      'repository_id', s.repository_id, 'binding_generation', s.binding_generation,
      'commit_oid', s.commit_oid, 'access', s.access))
FOR SHARE OF s, t
