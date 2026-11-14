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
-- One row per compile of the routing Markdown (spec 0011 §6). A failed
-- compile has no policy, so the digest is not the key; the same policy can
-- also be compiled again from a later revision.
CREATE TABLE routing_policies (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    structure_revision BIGINT NOT NULL CHECK(structure_revision >= 0),
    source_digest TEXT NOT NULL,
    -- The internal task compiling it; NULL when none could be submitted.
    internal_task_id UUID UNIQUE,
    status TEXT NOT NULL CHECK(status IN ('compiling','active','superseded','failed')),
    -- The compiled policy, when the answer parsed.
    digest TEXT,
    compiled JSONB,
    summary JSONB,
    -- Why a compile failed: each problem as `path: message`, then each
    -- sentence the compiler could not resolve; both also kept as lists.
    error TEXT,
    problems JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(problems) = 'array'),
    unresolved JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(unresolved) = 'array'),
    created_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    CHECK(status <> 'active' OR (digest IS NOT NULL AND compiled IS NOT NULL AND finished_at_ms IS NOT NULL))
);
-- At most one compiled policy routes at a time.
CREATE UNIQUE INDEX routing_policies_active ON routing_policies((TRUE)) WHERE status = 'active';
CREATE INDEX routing_policies_revision ON routing_policies(structure_revision);
CREATE TABLE route_needed_emissions (
    task_id UUID PRIMARY KEY,
    task_version BIGINT NOT NULL,
    emitted_at_ms BIGINT NOT NULL
);
UPDATE meta SET value='120' WHERE key='state_format_version';
