-- No implicit limits: the owner saves the first coherent matrix.
CREATE TABLE swarm_settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    active_mode TEXT NOT NULL DEFAULT 'normal' CHECK (active_mode IN ('normal','reduced')),
    normal JSONB,
    reduced JSONB,
    CHECK (normal IS NULL OR jsonb_typeof(normal) = 'array'),
    CHECK (reduced IS NULL OR jsonb_typeof(reduced) = 'array'),
    CHECK (active_mode <> 'reduced' OR reduced IS NOT NULL)
);
INSERT INTO swarm_settings (singleton) VALUES (TRUE);
CREATE TABLE swarm_setting_operations (
    operation_id UUID PRIMARY KEY,
    actor_id UUID NOT NULL REFERENCES users(id),
    request JSONB NOT NULL,
    receipt JSONB NOT NULL
);
CREATE TABLE swarm_waiting (
    task_id UUID PRIMARY KEY,
    provider TEXT NOT NULL,
    intelligence_class TEXT NOT NULL,
    reason TEXT NOT NULL,
    observed_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL
);
CREATE INDEX swarm_waiting_expiry ON swarm_waiting(expires_at_ms,task_id);
CREATE TABLE swarm_supervisor_admissions (
    actor_id UUID NOT NULL,
    generation BIGINT NOT NULL,
    provider TEXT NOT NULL,
    intelligence_class TEXT NOT NULL,
    PRIMARY KEY (actor_id,generation),
    FOREIGN KEY (actor_id,generation) REFERENCES central_session_generations(actor_id,generation)
        DEFERRABLE INITIALLY DEFERRED
);
-- Older supervisor hosts cannot bypass controls during a staggered rollout.
-- The same singleton lock serializes first-time configuration with a start;
-- existing generations remain unchanged and can renew, reconcile and stop.
CREATE FUNCTION guard_supervisor_swarm_admission() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE configured JSONB;
BEGIN
    SELECT normal INTO configured FROM swarm_settings WHERE singleton FOR UPDATE;
    IF configured IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM swarm_supervisor_admissions
        WHERE actor_id=NEW.actor_id AND generation=NEW.generation
    ) THEN
        RAISE EXCEPTION 'supervisor swarm admission required' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER supervisor_swarm_admission BEFORE INSERT ON central_session_generations
    FOR EACH ROW EXECUTE FUNCTION guard_supervisor_swarm_admission();
UPDATE meta SET value='115' WHERE key='state_format_version';
