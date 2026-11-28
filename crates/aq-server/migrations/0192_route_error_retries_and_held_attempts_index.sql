-- Two additions for the playbook driver under a wave of ready tasks.
--
-- 1. `route_needed_emissions.error_retries`: how many times the driver retired a
--    routing run that ended in an error (its command was refused for an
--    expired deadline, so nothing was done) and asked about the task again, for
--    the task version last asked about. Bounded by the driver Settings value
--    `route_error_retries`; the count restarts for a new task version. Additive
--    with a default; an older server ignores it.
-- 2. `execution_node_held`: the compute-occupancy read
--    (SELECT ... FROM execution_attempts WHERE node_id=$1 AND holds_task ORDER BY
--    attempt_id LIMIT 1025 FOR UPDATE) walked every attempt the node ever ran
--    through `execution_node_attempts` and discarded the settled ones, taking
--    1 to 5 s under load. A node holds few attempts at a time, so the partial
--    index answers it from the held rows only, in the same order.
SET LOCAL lock_timeout = '5s';
ALTER TABLE route_needed_emissions
    ADD COLUMN error_retries INTEGER NOT NULL DEFAULT 0 CHECK(error_retries >= 0);
CREATE INDEX execution_node_held ON execution_attempts(node_id, attempt_id) WHERE holds_task;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='192' WHERE key='state_format_version';
