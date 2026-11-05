-- D2: resumable owner repository onboarding. One row per caller operation_id
-- records the last completed step; no credential secret is ever stored here.
CREATE TABLE repository_onboardings (
    operation_id uuid PRIMARY KEY,
    actor_id uuid NOT NULL,
    request_digest text NOT NULL CHECK (request_digest ~ '^[0-9a-f]{64}$'),
    project_id uuid NOT NULL,
    repository_id uuid NOT NULL,
    step text NOT NULL CHECK (step IN ('started','registered','synced','checked','bound','refused')),
    state jsonb NOT NULL,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL
);
CREATE INDEX repository_onboardings_project ON repository_onboardings (project_id);
UPDATE meta SET value='79' WHERE key='state_format_version';
