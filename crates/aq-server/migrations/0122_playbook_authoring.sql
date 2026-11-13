-- Playbook authoring (spec 0013 §4.1, §4.5): an agent turns a Markdown source
-- into a playbook artifact. A source is the record the server keeps beside the
-- Markdown; the built-in routing source's Markdown lives in the structure
-- document, a registered source's here.
CREATE TABLE playbook_sources (
    id TEXT PRIMARY KEY CHECK (octet_length(id) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (kind IN ('builtin','registered')),
    -- The playbook id the source's artifact activates on.
    binding TEXT NOT NULL CONSTRAINT playbook_sources_binding_key UNIQUE
        CHECK (octet_length(binding) BETWEEN 1 AND 256),
    scope TEXT NOT NULL,
    purpose TEXT NOT NULL,
    -- The internal work purpose its judgments run under (spec 0013 §6.5).
    judgment_purpose TEXT NOT NULL,
    commands JSONB NOT NULL CHECK (jsonb_typeof(commands)='array'),
    events JSONB NOT NULL CHECK (jsonb_typeof(events)='array'),
    capabilities JSONB NOT NULL CHECK (jsonb_typeof(capabilities)='array'),
    -- Fact provider names, in the order the agent is shown them.
    facts JSONB NOT NULL CHECK (jsonb_typeof(facts)='array'),
    markdown TEXT,
    revision BIGINT NOT NULL CHECK (revision > 0),
    updated_at_ms BIGINT NOT NULL,
    -- The newest Markdown revision an authoring request has read: a request
    -- that read an older one is stale and records nothing.
    requested_revision BIGINT NOT NULL DEFAULT 0 CHECK (requested_revision >= 0),
    CHECK (kind <> 'registered' OR markdown IS NOT NULL)
);
-- One row per authoring of a source, recorded before its internal task is
-- submitted. A newer request supersedes a running one; at most one saved run
-- per source is active, and the previous ones stay for rollback.
CREATE TABLE playbook_authoring_runs (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id TEXT NOT NULL REFERENCES playbook_sources(id),
    source_revision BIGINT NOT NULL CHECK (source_revision >= 0),
    source_digest TEXT NOT NULL,
    facts_digest TEXT NOT NULL,
    -- The source record the run was authored under, without its revision.
    record_digest TEXT NOT NULL,
    internal_task_id UUID UNIQUE,
    status TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN ('running','saved','question','unable','refused','failed','superseded')),
    active BOOLEAN NOT NULL DEFAULT FALSE,
    artifact_digest TEXT,
    artifact JSONB,
    previous_digest TEXT,
    comparison JSONB,
    cases JSONB,
    question JSONB,
    unable JSONB,
    refusal TEXT,
    created_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    CHECK (NOT active OR status = 'saved'),
    CHECK (status <> 'saved' OR (artifact_digest IS NOT NULL AND artifact IS NOT NULL))
);
CREATE UNIQUE INDEX playbook_authoring_runs_active ON playbook_authoring_runs(source_id)
    WHERE status = 'saved' AND active;
CREATE INDEX playbook_authoring_runs_running ON playbook_authoring_runs(source_id, id)
    WHERE status = 'running';
CREATE INDEX playbook_authoring_runs_artifact ON playbook_authoring_runs(artifact_digest)
    WHERE artifact_digest IS NOT NULL;
-- The grant the server holds for a source on the owner's behalf (ruling R2):
-- one live grant per source. Its playbook_grants row never expires.
CREATE TABLE playbook_source_grants (
    source_id TEXT PRIMARY KEY REFERENCES playbook_sources(id),
    grant_id UUID UNIQUE NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    capabilities JSONB NOT NULL CHECK (jsonb_typeof(capabilities)='array'),
    created_at_ms BIGINT NOT NULL,
    revoked_at_ms BIGINT
);
UPDATE meta SET value='122' WHERE key='state_format_version';
