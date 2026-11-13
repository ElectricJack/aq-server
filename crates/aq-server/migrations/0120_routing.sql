-- Routing policy (spec 0011 §3, §4): every route a task has had, the
-- installation-wide switch, compiled policies and route-needed emissions.
-- The current route lives in the task's graph metadata (`aq:route`).
CREATE TABLE task_routes (
    task_id UUID NOT NULL,
    sequence BIGINT NOT NULL CHECK(sequence >= 1),
    -- NULL records a cleared route.
    route JSONB CHECK(route IS NULL OR jsonb_typeof(route) = 'object'),
    at_ms BIGINT NOT NULL,
    PRIMARY KEY(task_id, sequence)
);
CREATE TABLE routing_state (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    active_policy_digest TEXT
);
INSERT INTO routing_state(singleton) VALUES(TRUE);
CREATE TABLE routing_policies (
    digest TEXT PRIMARY KEY,
    structure_revision BIGINT,
    source_digest TEXT,
    compiled JSONB,
    summary JSONB,
    status TEXT NOT NULL CHECK(status IN ('active','superseded','failed')),
    error TEXT,
    internal_task_id UUID,
    created_at_ms BIGINT NOT NULL
);
CREATE TABLE route_needed_emissions (
    task_id UUID PRIMARY KEY,
    task_version BIGINT NOT NULL,
    emitted_at_ms BIGINT NOT NULL
);
UPDATE meta SET value='120' WHERE key='state_format_version';
