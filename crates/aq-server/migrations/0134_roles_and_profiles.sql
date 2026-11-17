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
    limits_revision BIGINT NOT NULL DEFAULT 0,
    -- The routing document (spec 0014 §5.2): the text the routing playbook is
    -- authored from. NULL until first saved. Not part of any snapshot.
    routing_markdown TEXT,
    routing_revision BIGINT NOT NULL DEFAULT 0
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
-- The snapshot a live attempt launched with. A pinned snapshot is not rolled
-- off. An attempt is live while it holds its task (`holds_task`): the pin is
-- written with the attempt, and aq_role_settings releases the pins of
-- attempts that no longer hold their task (settled, cancelled, abandoned,
-- lost and released, never granted, or removed) whenever an attempt is
-- created and whenever a snapshot is activated, before it rolls. The attempt
-- tables' triggers are sealed by earlier formats, so this is not a trigger.
-- The summary below is what remains afterwards.
CREATE TABLE role_snapshot_pins (
    attempt_id UUID PRIMARY KEY,
    digest TEXT NOT NULL REFERENCES role_snapshots(digest)
);
CREATE INDEX role_snapshot_pins_digest ON role_snapshot_pins(digest);
-- What an attempt ran with (spec 0014 §7.1): role, level, provider, model,
-- reasoning, harness, execution profile and snapshot digest; a few hundred
-- bytes. `snapshot_digest` is JSON null for an attempt recorded before roles
-- and profiles (filled below): no snapshot of that time exists. One row per attempt, written in the transaction that creates the
-- attempt and removed with the attempt's record (aq_role_settings::attempts).
-- The attempt tables' own shape is sealed by earlier formats, so the summary
-- has its own table keyed by the attempt.
CREATE TABLE attempt_ran_with (
    attempt_id UUID PRIMARY KEY,
    ran_with JSONB NOT NULL CHECK(jsonb_typeof(ran_with) = 'object')
);
-- Why the last offer of a task was refused before any attempt existed
-- (spec 0014 §6.1): a startup text over its byte limit, or an environment
-- with no platform text. One row per task; the work read shows it as a
-- readiness reason. The row goes when a later offer of the task composes, and
-- the table is a rolling set (aq_role_settings::refusals keeps the newest).
CREATE TABLE task_offer_refusals (
    task_id UUID PRIMARY KEY,
    project_id UUID NOT NULL,
    refusal JSONB NOT NULL CHECK(jsonb_typeof(refusal) = 'object'),
    recorded_at_ms BIGINT NOT NULL
);
CREATE INDEX task_offer_refusals_recent ON task_offer_refusals(recorded_at_ms);
-- == The profile catalog is removed ==
-- Everything above only creates the role tables. Everything below removes
-- the old way (spec 0014 §9, §10) and is not reversible: the old catalog
-- bodies, measurements and class structure are not carried over. A database
-- backup is taken before this migration runs.

-- The owner's routing document moves from the Intelligence structure to its
-- own place in the role store, as its first saved revision. Parents and
-- assignments are not carried: a level's id is the router's class.
UPDATE role_settings r
   SET routing_markdown = s.document->>'routing_markdown',
       routing_revision = 1,
       revision = 1
  FROM intelligence_structure s
 WHERE btrim(COALESCE(s.document->>'routing_markdown', '')) <> '';

DROP TABLE profile_catalog_environment_templates;
DROP TABLE profile_catalog_operations;
DROP TABLE profile_catalog_registry;
DROP TABLE profile_catalog_artifacts;
DROP TABLE context_qualification_renewals;
DROP TABLE intelligence_structure_operations;
DROP TABLE intelligence_structure;

-- Project bindings (spec 0014 §5.4): the catalog digest and the three profile
-- allowlists go. This migration cannot know a project's defaults, so a
-- binding that names none gets `developer` and `standard-high`; ceilings,
-- nodes, groups, environments, repository and graph references and
-- publication are kept. The same rewrite is applied to every stored copy a
-- read returns: the history records and the save receipts.
CREATE FUNCTION pg_temp.binding_0134(b JSONB) RETURNS JSONB LANGUAGE sql IMMUTABLE AS $$
  SELECT CASE WHEN jsonb_typeof(b) = 'object' THEN
    jsonb_build_object('default_role', 'developer', 'default_intelligence', 'standard-high')
    || (b - 'catalog_digest' - 'allowed_agent_profiles'
          - 'allowed_intelligence_profiles' - 'allowed_execution_profiles')
  ELSE b END
$$;
UPDATE project_execution_bindings SET binding = pg_temp.binding_0134(binding);
UPDATE project_execution_binding_history
   SET record = jsonb_set(record, '{binding}', pg_temp.binding_0134(record->'binding'))
 WHERE jsonb_typeof(record->'binding') = 'object';
UPDATE project_execution_operations
   SET response = jsonb_set(response, '{record,binding}', pg_temp.binding_0134(response#>'{record,binding}'))
 WHERE jsonb_typeof(response#>'{record,binding}') = 'object';
DROP FUNCTION pg_temp.binding_0134(JSONB);

-- Attempts from before roles and profiles get their summary from the launch
-- manifest they already hold: provider, model, reasoning and harness as
-- launched; role, level and execution profile are the old agent,
-- intelligence and execution profile ids without their namespace prefix and
-- `@version`. The snapshot digest is null and no snapshot is pinned. An
-- attempt without a profiled launch, or whose manifest lacks one of these
-- values, gets no summary rather than an invented one.
INSERT INTO attempt_ran_with(attempt_id, ran_with)
SELECT attempt_id, jsonb_build_object(
         'role', role, 'level', level, 'provider', provider, 'model', model,
         'reasoning', reasoning, 'harness', harness,
         'execution_profile', execution_profile, 'snapshot_digest', NULL)
  FROM (SELECT attempt_id,
               regexp_replace(regexp_replace(launch_manifest#>>'{agent_profile,id}', '^[a-z]+:', ''), '@[0-9]+$', '') AS role,
               regexp_replace(regexp_replace(launch_manifest#>>'{intelligence_profile,id}', '^[a-z]+:', ''), '@[0-9]+$', '') AS level,
               regexp_replace(regexp_replace(launch_manifest#>>'{execution_profile,id}', '^[a-z]+:', ''), '@[0-9]+$', '') AS execution_profile,
               launch_manifest#>>'{model,provider}' AS provider,
               launch_manifest#>>'{model,model}' AS model,
               launch_manifest#>>'{model,reasoning}' AS reasoning,
               launch_manifest#>>'{harness,harness}' AS harness
          FROM profiled_launches) l
 WHERE role IS NOT NULL AND level IS NOT NULL AND execution_profile IS NOT NULL
   AND provider IS NOT NULL AND model IS NOT NULL AND reasoning IS NOT NULL
   AND harness IS NOT NULL
ON CONFLICT (attempt_id) DO NOTHING;

-- A node's stored inventory listed the models of each harness. The key is
-- gone from the harness shape (any model a level names is allowed), so it is
-- stripped; the worker's next inventory replaces the row anyway.
UPDATE intelligence_node_snapshots s
   SET snapshot = jsonb_set(s.snapshot, '{harnesses}',
         (SELECT COALESCE(jsonb_agg(h - 'models' ORDER BY n), '[]'::jsonb)
            FROM jsonb_array_elements(s.snapshot->'harnesses') WITH ORDINALITY AS e(h, n)))
 WHERE jsonb_typeof(s.snapshot->'harnesses') = 'array'
   AND EXISTS (SELECT 1 FROM jsonb_array_elements(s.snapshot->'harnesses') h WHERE h ? 'models');

-- A supervisor keeps its control and its choice. The stored choice becomes a
-- level id and the chosen candidate (provider, model, reasoning, harness);
-- `enabled` and the revision are untouched. The old profile id is the level
-- id, except the two renamed classes (`fast-low` is `simple-low`, `fast-high`
-- is `simple-high`); an id with no level of that name keeps its id. No
-- snapshot digest is stored: the server resolves the level and candidate
-- against the active snapshot each time the control is read or the
-- supervisor starts, and says why when the level does not list them.
UPDATE supervisor_controls
   SET profile = jsonb_strip_nulls(jsonb_build_object(
         'profile_id', CASE profile->>'profile_id'
                         WHEN 'fast-low' THEN 'simple-low'
                         WHEN 'fast-high' THEN 'simple-high'
                         ELSE profile->>'profile_id' END,
         'candidate_id', concat_ws(':', profile->>'provider', profile->>'model', profile->>'reasoning'),
         'name', profile->'name',
         'provider', profile->'provider',
         'model', profile->'model',
         'reasoning', profile->'reasoning',
         'harness', profile->'harness'))
 WHERE jsonb_typeof(profile) = 'object';

UPDATE meta SET value='134' WHERE key='state_format_version';
