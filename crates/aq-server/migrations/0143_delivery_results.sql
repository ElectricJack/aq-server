-- Epic 0, delivery by playbook (spec 0016 sections 6.1 and 6.2): what the host
-- records so that it can send `aq.work.result_ready` and answer `work.read`
-- with the facts of a task's latest result. Additive: a new table each, and
-- profile 10 (AQ host catalog 2.3.0) admitted as a namespace profile; no row
-- is touched, so an older server keeps running on the migrated schema.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8,9,10));

-- One row per task the host has sent `aq.work.result_ready` for. `returns` is
-- how many times a playbook sent the task back with `work.return` (counted by
-- the host, never by the playbook). emitted_* say which result the event was
-- last sent for, when, and under which facts (main and the pull request), so
-- the event is repeated at an interval and also when those facts move.
CREATE TABLE delivery_tasks (
    task_id UUID PRIMARY KEY,
    returns INTEGER NOT NULL DEFAULT 0 CHECK(returns BETWEEN 0 AND 1000),
    emitted_result_id UUID,
    emitted_at_ms BIGINT NOT NULL DEFAULT 0,
    emitted_facts TEXT,
    -- When the row was last looked at without sending; the emitter looks at
    -- the rows not looked at for longest first.
    checked_at_ms BIGINT NOT NULL DEFAULT 0
);

-- A playbook's decision on a result: `work.complete` (completed),
-- `work.return` (returned) or `work.hold` (held). A result with a decision
-- is not announced again. Written by those commands.
CREATE TABLE delivery_decisions (
    result_id UUID PRIMARY KEY,
    task_id UUID NOT NULL,
    decision TEXT NOT NULL CHECK(decision IN ('completed','returned','held')),
    reason TEXT CHECK(reason IS NULL OR length(reason) BETWEEN 1 AND 64),
    decided_at_ms BIGINT NOT NULL CHECK(decided_at_ms > 0)
);
CREATE INDEX delivery_decisions_task ON delivery_decisions(task_id);

-- The commit main pointed at when central aq-git last synced a project's
-- default branch (`work.read` `main_head`). Written by the main sync.
CREATE TABLE delivery_main_heads (
    project_id UUID PRIMARY KEY,
    head TEXT NOT NULL CHECK(head ~ '^[0-9a-f]{40}$'),
    observed_at_ms BIGINT NOT NULL CHECK(observed_at_ms > 0)
);

-- The pull request of a task as the host last knew it (`work.read`
-- `pull_request`). Written by the pull_request commands.
CREATE TABLE delivery_pull_requests (
    task_id UUID PRIMARY KEY,
    number BIGINT NOT NULL CHECK(number BETWEEN 1 AND 1000000000),
    url TEXT NOT NULL CHECK(length(url) <= 2048),
    state TEXT NOT NULL CHECK(state IN ('open','closed','merged')),
    mergeable TEXT NOT NULL CHECK(mergeable IN ('mergeable','conflicting','unknown')),
    checks_state TEXT NOT NULL CHECK(checks_state IN ('none','pending','passed','failed')),
    checks_failed JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(checks_failed) = 'array'),
    updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms > 0)
);
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='143' WHERE key='state_format_version';
