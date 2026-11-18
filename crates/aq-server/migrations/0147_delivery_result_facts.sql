-- Epic 0, delivery by playbook (spec 0016 section 7): the facts the result
-- path records for each result with the cargo feature `delivery_playbook`,
-- in place of a settlement verdict: the attempt's fences, the retained
-- source and whether it changed, the check and delivery evidence. One row
-- per result report, replaced when the same report is recorded again.
-- Nothing reads a decision from it. Additive: a new table; no row is
-- touched, so an older server keeps running on the migrated schema.
SET LOCAL lock_timeout = '5s';
CREATE TABLE delivery_result_facts (
    report_id UUID PRIMARY KEY,
    attempt_id UUID NOT NULL,
    task_id UUID NOT NULL,
    facts JSONB NOT NULL CHECK(jsonb_typeof(facts) = 'object' AND octet_length(facts::text) <= 16384),
    recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms > 0),
    updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms >= recorded_at_ms)
);
CREATE INDEX delivery_result_facts_task ON delivery_result_facts(task_id);
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='147' WHERE key='state_format_version';
