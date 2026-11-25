WITH evidence AS (
    SELECT e.attempt_id, count(*) FILTER (
        WHERE e.decision->>'kind'='unresolved' OR e.record#>>'{body,kind,kind}'='gap'
    ) AS unresolved
    FROM observation_events e
    WHERE e.recorded_at_ms BETWEEN $3 AND $4
      AND (e.decision->>'kind' IN ('append', 'unresolved') OR e.record#>>'{body,kind,kind}'='gap')
      AND e.decision#>>'{supersedes,event_id}' IS NULL
    GROUP BY e.attempt_id
), activity AS (
    SELECT attempt_id, unresolved, true AS expected FROM evidence
    UNION ALL
    SELECT attempt_id, 0::bigint, true FROM profiled_started_receipts
    WHERE performance_started_at(receipt_bytes) BETWEEN $3 AND $4
    UNION ALL
    SELECT attempt_id, 0::bigint, false FROM execution_attempts WHERE task_id=$2
)
SELECT a.attempt_id, sum(activity.unresolved)::bigint AS unresolved,
       bool_or(activity.expected) AS expected,
       s.selection#>>'{choice,model}' AS intended_model,
       head.state->>'status' AS attempt_status
FROM activity
JOIN execution_attempts a USING(attempt_id)
LEFT JOIN profiled_launches l USING(attempt_id)
LEFT JOIN admission_source_observations so ON so.observation_id=l.source_observation_id
LEFT JOIN intelligence_attempt_selections s USING(attempt_id)
LEFT JOIN profiled_authority_hot head USING(attempt_id)
WHERE ($1::uuid IS NULL OR so.project_id=$1)
  AND ($2::uuid IS NULL OR a.task_id=$2)
  AND coalesce(
      (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::bigint,
      (s.selection->>'selected_at_ms')::bigint,
      0
  )<=$4
GROUP BY a.attempt_id, s.selection#>>'{choice,model}', head.state->>'status'
ORDER BY a.attempt_id
LIMIT $5;
