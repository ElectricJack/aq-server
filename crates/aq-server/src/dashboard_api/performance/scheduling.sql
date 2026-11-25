WITH window_claims AS (
    SELECT l.attempt_id, a.task_id, l.launch_binding->>'assignment_generation' AS generation,
           (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT AS recorded_at_ms
    FROM profiled_launches l JOIN execution_attempts a USING (attempt_id)
    WHERE a.kind = 'profiled_v1'
      AND (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT BETWEEN $1 AND $2
), coverage AS (
    SELECT count(*) AS claims, count(t.attempt_id) AS covered_claims,
           percentile_disc(.5) WITHIN GROUP (ORDER BY t.claimed_at_ms - t.eligible_at_ms) AS p50,
           percentile_disc(.95) WITHIN GROUP (ORDER BY t.claimed_at_ms - t.eligible_at_ms) AS p95
    FROM window_claims c
    LEFT JOIN performance_scheduling_timings t
        ON t.attempt_id = c.attempt_id AND t.task_id = c.task_id AND t.generation::text = c.generation
        AND t.claimed_at_ms >= c.recorded_at_ms AND t.claimed_at_ms <= $2
), counts AS (
    SELECT reason, sum(count) AS count FROM (
        SELECT reason, count FROM performance_scheduling_minute
        WHERE minute_ms >= $1 AND minute_ms + 59999 <= $2 AND reason <> 'dropped_observation'
        UNION ALL
        SELECT reason, count(*) FROM performance_scheduling_decisions
        WHERE observed_at_ms BETWEEN $1 AND $2
          AND (observed_at_ms / 60000 * 60000 < $1 OR observed_at_ms / 60000 * 60000 + 59999 > $2)
        GROUP BY reason
    ) observations GROUP BY reason
), examples AS (
    SELECT item
    FROM performance_scheduling_examples CROSS JOIN LATERAL jsonb_array_elements(example) AS item
    WHERE minute_ms BETWEEN $1 / 60000 * 60000 AND $2
      AND (item->>'observed_at_ms')::BIGINT BETWEEN $1 AND $2
    ORDER BY (item->>'observed_at_ms')::BIGINT DESC, item->>'task_id', item->>'reason'
    LIMIT 8
)
SELECT jsonb_build_object(
    'reason_counts', coalesce((SELECT jsonb_agg(to_jsonb(counts) ORDER BY reason) FROM counts), '[]'::jsonb),
    'examples', coalesce((SELECT jsonb_agg(item) FROM examples), '[]'::jsonb),
    'claims', claims, 'covered_claims', covered_claims,
    'p50_eligible_to_claim_ms', p50, 'p95_eligible_to_claim_ms', p95,
    'dropped_observations', coalesce((
        SELECT sum(count) FROM performance_scheduling_minute
        WHERE reason = 'dropped_observation' AND minute_ms BETWEEN $1 / 60000 * 60000 AND $2
    ), 0)
) FROM coverage;
