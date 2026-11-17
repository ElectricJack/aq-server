-- Roles and profiles (spec 0014 §7, §7.1): roles, intelligence profiles and
-- execution profiles as Settings documents, the snapshot each valid set
-- compiles into, and which snapshot is active. Owned by aq-role-settings.
--
-- No limit and no default value is written here: a NULL selection_defaults or
-- limits means "never saved", and the server's own defaults apply.
CREATE TABLE role_snapshots (
    digest TEXT PRIMARY KEY,
    -- Canonical bytes; the digest is their SHA-256.
    snapshot BYTEA NOT NULL,
    created_at_ms BIGINT NOT NULL,
    -- The store revision that last made this snapshot the active one. The
    -- rolling set keeps the most recently activated.
    activated_revision BIGINT NOT NULL
);
CREATE TABLE role_documents (
    kind TEXT NOT NULL CHECK(kind IN ('role','intelligence','execution')),
    id TEXT NOT NULL,
    markdown TEXT NOT NULL,
    -- The store revision at which this document was last saved.
    revision BIGINT NOT NULL CHECK(revision > 0),
    updated_at_ms BIGINT NOT NULL,
    PRIMARY KEY(kind, id)
);
CREATE TABLE role_settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
    revision BIGINT NOT NULL DEFAULT 0 CHECK(revision >= 0),
    active_digest TEXT REFERENCES role_snapshots(digest),
    selection_defaults JSONB CHECK(selection_defaults IS NULL OR jsonb_typeof(selection_defaults) = 'object'),
    selection_defaults_revision BIGINT NOT NULL DEFAULT 0,
    -- Document sizes and how many snapshots and save receipts are kept.
    limits JSONB CHECK(limits IS NULL OR jsonb_typeof(limits) = 'object'),
    limits_revision BIGINT NOT NULL DEFAULT 0
);
INSERT INTO role_settings(singleton) VALUES(TRUE);
-- The shipped default documents this database has installed. A default is
-- installed once: deleting its document does not bring it back at the next
-- start, and a default added by a later release installs once.
CREATE TABLE role_default_installs (
    kind TEXT NOT NULL CHECK(kind IN ('role','intelligence','execution')),
    id TEXT NOT NULL,
    installed_at_ms BIGINT NOT NULL,
    PRIMARY KEY(kind, id)
);
-- Save receipts for repeated requests; a rolling set, pruned on every save.
CREATE TABLE role_settings_operations (
    operation_id UUID PRIMARY KEY,
    actor_id UUID NOT NULL REFERENCES users(id),
    request_digest TEXT NOT NULL,
    receipt JSONB NOT NULL,
    revision BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL
);
CREATE INDEX role_settings_operations_recent ON role_settings_operations(revision);
-- The snapshot a kept attempt launched with. A pinned snapshot is not rolled
-- off; the pin goes when the attempt's record does.
CREATE TABLE role_snapshot_pins (
    attempt_id UUID PRIMARY KEY,
    digest TEXT NOT NULL REFERENCES role_snapshots(digest)
);
CREATE INDEX role_snapshot_pins_digest ON role_snapshot_pins(digest);
-- What an attempt ran with (spec 0014 §7.1): role, level, provider, model,
-- reasoning, harness, execution profile and snapshot digest; a few hundred
-- bytes. One row per attempt, written in the transaction that creates the
-- attempt and removed with the attempt's record (aq_role_settings::attempts).
-- The attempt tables' own shape is sealed by earlier formats, so the summary
-- has its own table keyed by the attempt.
CREATE TABLE attempt_ran_with (
    attempt_id UUID PRIMARY KEY,
    ran_with JSONB NOT NULL CHECK(jsonb_typeof(ran_with) = 'object')
);
UPDATE meta SET value='134' WHERE key='state_format_version';
