WITH input AS MATERIALIZED (
    SELECT * FROM jsonb_to_recordset($1) AS x(
        operation_id UUID, task_id UUID, reason TEXT, observed_at_ms BIGINT,
        attempt_id UUID, generation BIGINT, eligible_at_ms BIGINT, claimed_at_ms BIGINT
    )
), expired_decisions AS (
    DELETE FROM performance_scheduling_decisions WHERE observed_at_ms < $2
), expired_counts AS (
    DELETE FROM performance_scheduling_minute WHERE minute_ms < $2
), expired_examples AS (
    DELETE FROM performance_scheduling_examples WHERE minute_ms < $2
), expired_timings AS (
    DELETE FROM performance_scheduling_timings WHERE claimed_at_ms < $2
), decisions AS (
    INSERT INTO performance_scheduling_decisions (operation_id, task_id, reason, observed_at_ms)
    SELECT operation_id, coalesce(task_id, '00000000-0000-0000-0000-000000000000'),
           reason, observed_at_ms
    FROM input
    WHERE operation_id IS NOT NULL AND observed_at_ms >= $2
    ORDER BY observed_at_ms
    ON CONFLICT DO NOTHING
    RETURNING task_id, reason, observed_at_ms, observed_at_ms / 60000 * 60000 AS minute_ms
), timings AS (
    INSERT INTO performance_scheduling_timings
        (attempt_id, task_id, generation, eligible_at_ms, claimed_at_ms)
    SELECT attempt_id, task_id, generation, eligible_at_ms, claimed_at_ms
    FROM input
    WHERE attempt_id IS NOT NULL AND claimed_at_ms >= $2
    ON CONFLICT DO NOTHING
), examples AS (
    SELECT *, row_number() OVER (PARTITION BY minute_ms ORDER BY observed_at_ms DESC, task_id, reason) AS rank
    FROM decisions
    WHERE task_id <> '00000000-0000-0000-0000-000000000000'
), sample AS (
    INSERT INTO performance_scheduling_examples (minute_ms, example)
    SELECT minute_ms, jsonb_agg(jsonb_build_object('task_id', task_id, 'reason', reason,
                                                  'observed_at_ms', observed_at_ms) ORDER BY rank)
    FROM examples WHERE rank <= 8 GROUP BY minute_ms
    ON CONFLICT (minute_ms) DO UPDATE
    SET example = (
        SELECT jsonb_agg(item ORDER BY rank)
        FROM (
            SELECT item, row_number() OVER (ORDER BY (item->>'observed_at_ms')::BIGINT DESC,
                                            item->>'task_id', item->>'reason') AS rank
            FROM jsonb_array_elements(excluded.example || performance_scheduling_examples.example) item
            ORDER BY rank LIMIT 8
        ) latest
    )
)
INSERT INTO performance_scheduling_minute (minute_ms, reason, count)
SELECT minute_ms, reason, count(*) FROM decisions GROUP BY minute_ms, reason
UNION ALL SELECT $3, 'dropped_observation', $4 WHERE $4 > 0
ON CONFLICT (minute_ms, reason) DO UPDATE
SET count = performance_scheduling_minute.count + excluded.count;
