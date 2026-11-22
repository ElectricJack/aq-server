-- Spec 0017 §6.3, §6.7: environment versions and builder jobs move from the
-- boot config into the database, and the environment limits are a Settings
-- value. Additive: an older server keeps running on the migrated schema.
--
-- A base environment (the AQ base template) is a version with no project.
CREATE TABLE environment_versions (
    environment_id TEXT NOT NULL CHECK (environment_id ~ '^[A-Za-z0-9_.:-]{1,128}$'),
    version TEXT NOT NULL CHECK (version ~ '^[A-Za-z0-9_.:-]{1,128}$'),
    project_id UUID,
    template_digest TEXT NOT NULL CHECK (template_digest ~ '^sha256:[0-9a-f]{64}$'),
    image TEXT NOT NULL CHECK (octet_length(image) BETWEEN 1 AND 512),
    provides JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(provides) = 'array'),
    platform_text TEXT CHECK (octet_length(platform_text) <= 65536),
    toolchain JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(toolchain) = 'array'),
    warm JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(warm) = 'array'),
    harnesses JSONB NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(harnesses) = 'array'),
    recipe_sha256 TEXT CHECK (recipe_sha256 ~ '^[0-9a-f]{64}$'),
    -- The base this version was built on; NULL for a base itself.
    base_digest TEXT CHECK (base_digest ~ '^sha256:[0-9a-f]{64}$'),
    admitted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (environment_id, version)
);
CREATE INDEX environment_versions_project ON environment_versions(project_id, environment_id);

-- One build or try for a builder node (`run` is a try's command, if any).
-- Rows are rolling records.
CREATE TABLE environment_jobs (
    job_id UUID PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('build', 'try')),
    project_id UUID NOT NULL,
    -- The project's repository as nodes key it (the binding's), taken when
    -- the job was queued: the only one its fetch-only git grant reaches.
    repository_id UUID NOT NULL,
    commit TEXT NOT NULL CHECK (commit ~ '^[0-9a-f]{40}$'),
    recipes JSONB NOT NULL CHECK (jsonb_typeof(recipes) = 'array'),
    run TEXT,
    -- The base the job builds on, chosen when it was queued; the builder
    -- publishes to the repository its own builder config names.
    base_template TEXT NOT NULL CHECK (base_template ~ '^[^@]+@sha256:[0-9a-f]{64}$' AND octet_length(base_template) <= 512),
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'claimed', 'completed')),
    claimed_by UUID,
    -- A claim is a lease of `environment_limits.build_wall_ms`: once it runs
    -- out another builder may claim the job, and the old claimer's late
    -- completion is refused.
    claimed_at TIMESTAMPTZ,
    lease_until TIMESTAMPTZ,
    -- Claims so far; past `environment_limits.max_job_claims` without a
    -- result the server completes the job as failed.
    claims INTEGER NOT NULL DEFAULT 0,
    result JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Rolling records: completing a job deletes the project's completed jobs
    -- of that kind beyond the newest `environment_limits.keep_versions`.
    completed_at TIMESTAMPTZ
);
CREATE INDEX environment_jobs_queued ON environment_jobs(created_at) WHERE state = 'queued';
-- The claim's other half: a claimed job whose lease ran out.
CREATE INDEX environment_jobs_leased ON environment_jobs(lease_until) WHERE state = 'claimed';
CREATE INDEX environment_jobs_completed ON environment_jobs(project_id, kind, completed_at) WHERE state = 'completed';

-- The template digests each node has pulled and can run (rolling: each
-- report replaces the node's previous set; only admitted digests are kept).
CREATE TABLE environment_ready (
    node_id UUID PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    digests JSONB NOT NULL CHECK (jsonb_typeof(digests) = 'array'),
    reported_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- NULL means never saved: the defaults of the running build apply.
ALTER TABLE role_settings
    ADD COLUMN environment_limits JSONB,
    ADD COLUMN environment_limits_revision BIGINT NOT NULL DEFAULT 0;

UPDATE meta SET value='164' WHERE key='state_format_version';
