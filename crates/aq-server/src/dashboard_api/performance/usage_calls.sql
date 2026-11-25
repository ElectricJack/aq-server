-- Corrections retain the first canonical receipt's reporting window and the
-- original owning attempt.
WITH in_window AS (
    SELECT f.call_key, e.event_id, e.recorded_at_ms, e.sequence,
           h.owner_attempt_id, so.project_id IS NULL AS unassigned,
           s.selection#>>'{choice,harness}' AS harness
    FROM observation_events e
    JOIN observation_facts f USING(event_id)
    JOIN observation_call_heads h USING(call_key)
    JOIN execution_attempts a ON a.attempt_id=h.owner_attempt_id
    LEFT JOIN profiled_launches l ON l.attempt_id=a.attempt_id
    LEFT JOIN admission_source_observations so ON so.observation_id=l.source_observation_id
    LEFT JOIN intelligence_attempt_selections s ON s.attempt_id=a.attempt_id
    WHERE e.recorded_at_ms BETWEEN $3 AND $4
      AND ($1::uuid IS NULL OR so.project_id=$1)
      AND ($2::uuid IS NULL OR a.task_id=$2)
      AND e.decision#>>'{supersedes,event_id}' IS NULL
    ORDER BY e.recorded_at_ms DESC,e.sequence DESC,e.event_id DESC
    LIMIT $5
)
SELECT w.owner_attempt_id, w.unassigned, w.harness, latest.fact, latest.billing
FROM in_window w
CROSS JOIN LATERAL (
    SELECT f.fact, src.authority->'billing' AS billing
    FROM observation_facts f
    JOIN observation_events e USING(event_id)
    LEFT JOIN observation_sources src ON src.node_id=e.node_id
      AND src.source_id=(e.record#>>'{body,provenance,source_id}')::uuid
      AND src.generation=(e.record#>>'{body,provenance,source_generation}')::bigint
    WHERE f.call_key=w.call_key AND e.recorded_at_ms<=$4
    ORDER BY e.recorded_at_ms DESC,e.sequence DESC,e.event_id DESC LIMIT 1
) latest;
