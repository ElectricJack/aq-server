-- Generation one is enforced under the producer's per-task publication lock.
-- A later selection cannot fill an absent original launch or replace it.
SELECT jsonb_build_object(
    'attempt_id', a.attempt_id,
    'candidate', s.selection#>>'{choice,candidate_id}',
    'intended_model', s.selection#>>'{choice,model}',
    'policy_revision', d.authority_policy_revision,
    'registry_revision', (s.selection->>'registry_revision')::bigint,
    'recorded_at_ms', (s.selection->>'selected_at_ms')::bigint
)
FROM execution_attempts a
JOIN profiled_launches l USING(attempt_id)
JOIN intelligence_attempt_selections s USING(attempt_id)
JOIN intelligence_decisions d USING(decision_id)
JOIN admission_source_observations so ON so.observation_id=l.source_observation_id
WHERE a.task_id=$1 AND ($2::uuid IS NULL OR so.project_id=$2)
  AND l.launch_binding->>'assignment_generation'='1'
  AND coalesce(
      (l.graph_task_observation#>>'{native_offer,native,offered,clock,server_time_ms}')::bigint,
      (s.selection->>'selected_at_ms')::bigint
  )<=$3
  AND s.selection#>>'{choice,candidate_id}' IS NOT NULL
  AND s.selection#>>'{choice,model}' IS NOT NULL
ORDER BY (s.selection->>'selected_at_ms')::bigint, a.attempt_id
LIMIT 1;
