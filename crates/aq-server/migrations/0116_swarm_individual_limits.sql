-- Explicit nullable overrides preserve all existing mode matrices and defaults.
ALTER TABLE swarm_settings ADD COLUMN limits JSONB NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(limits) = 'object');
ALTER TABLE swarm_supervisor_admissions ADD COLUMN admission_version INTEGER NOT NULL DEFAULT 1;
CREATE OR REPLACE FUNCTION guard_supervisor_swarm_admission() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE configured BOOLEAN; independent BOOLEAN;
BEGIN
    SELECT EXISTS (
        SELECT 1 FROM jsonb_array_elements(COALESCE(limits->'provider_limits','[]'::jsonb)) p
        WHERE p->>'limit' IS NOT NULL
    ) OR EXISTS (
        SELECT 1 FROM jsonb_array_elements(COALESCE(limits->'cell_limits','[]'::jsonb)) c
        WHERE c->>'limit' IS NOT NULL
    ) INTO independent FROM swarm_settings WHERE singleton FOR UPDATE;
    SELECT normal IS NOT NULL OR independent INTO configured
        FROM swarm_settings WHERE singleton;
    IF configured AND NOT EXISTS (
        SELECT 1 FROM swarm_supervisor_admissions
        WHERE actor_id=NEW.actor_id AND generation=NEW.generation
          AND (NOT independent OR admission_version >= 2)
    ) THEN
        RAISE EXCEPTION 'supervisor swarm admission required' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
UPDATE meta SET value='116' WHERE key='state_format_version';
