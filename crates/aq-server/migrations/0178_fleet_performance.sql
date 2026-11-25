-- Native AQ Performance reporting. All new evidence is additive.
-- The fleet row is the commit marker for a complete, atomic minute tick.
CREATE TABLE performance_fleet_minutes (
    minute_ms BIGINT PRIMARY KEY CHECK (minute_ms >= 0 AND minute_ms % 60000 = 0),
    observed_at_ms BIGINT NOT NULL CHECK (observed_at_ms >= minute_ms AND observed_at_ms - minute_ms < 60000),
    inventory_complete BOOLEAN NOT NULL,
    ready_count BIGINT CHECK (ready_count >= 0),
    ready_accuracy TEXT NOT NULL CHECK (ready_accuracy IN ('exact', 'lower_bound', 'unknown')),
    CHECK ((ready_count IS NULL) = (ready_accuracy = 'unknown'))
);

CREATE TABLE performance_machine_minutes (
    minute_ms BIGINT NOT NULL REFERENCES performance_fleet_minutes(minute_ms) ON DELETE CASCADE,
    machine_id TEXT NOT NULL,
    label TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('online', 'partial', 'offline', 'stale', 'paused')),
    enabled_slots BIGINT NOT NULL CHECK (enabled_slots >= 0),
    occupied_slots BIGINT NOT NULL CHECK (occupied_slots >= 0),
    free_slots BIGINT NOT NULL CHECK (free_slots BETWEEN 0 AND enabled_slots),
    enabled_occupied_slots BIGINT NOT NULL CHECK (enabled_occupied_slots BETWEEN 0 AND occupied_slots),
    PRIMARY KEY (minute_ms, machine_id)
);

-- Immutable reporting preferences. The empty class is a project default.
-- A null definition restores inheritance, retaining earlier revisions.
CREATE TABLE performance_delivery_definitions (
    project_id UUID NOT NULL REFERENCES project_execution_bindings(project_id),
    work_class TEXT NOT NULL CHECK (length(work_class) <= 128),
    revision BIGINT NOT NULL CHECK (revision > 0),
    effective_at_ms BIGINT NOT NULL CHECK (effective_at_ms > 0),
    definition JSONB CHECK (jsonb_typeof(definition) = 'object'),
    operation_id UUID NOT NULL UNIQUE,
    actor_id UUID NOT NULL REFERENCES users(id),
    PRIMARY KEY (project_id,work_class,revision)
);
CREATE TRIGGER performance_delivery_definition_immutable
    BEFORE UPDATE OR DELETE ON performance_delivery_definitions
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

-- Only authoritative Graph first-ready evidence belongs here. A ready page
-- or an admission/offer observation cannot supply this timestamp or class.
CREATE TABLE performance_task_first_ready (
    project_id UUID NOT NULL,
    task_id UUID NOT NULL,
    ready_at_ms BIGINT NOT NULL CHECK (ready_at_ms > 0),
    work_class TEXT CHECK (length(work_class) BETWEEN 1 AND 128),
    PRIMARY KEY (project_id,task_id)
);
CREATE TRIGGER performance_task_first_ready_immutable
    BEFORE UPDATE OR DELETE ON performance_task_first_ready
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

-- Pilot completion receipts have no timestamp. Capture the observation
-- atomically for new confirmations, including on an older running server.
CREATE TABLE performance_pilot_completion_times (
    operation_id UUID PRIMARY KEY REFERENCES profiled_task_completions(operation_id),
    confirmed_at_ms BIGINT NOT NULL CHECK (confirmed_at_ms > 0)
);
CREATE FUNCTION performance_pilot_completion_clock() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO performance_pilot_completion_times
        VALUES (NEW.operation_id, (extract(epoch FROM clock_timestamp()) * 1000)::bigint);
    RETURN NULL;
END $$;
CREATE TRIGGER performance_pilot_completion_clock AFTER UPDATE OF state ON profiled_task_completions
    FOR EACH ROW WHEN (OLD.state <> 'confirmed' AND NEW.state = 'confirmed')
    EXECUTE FUNCTION performance_pilot_completion_clock();

CREATE INDEX performance_auto_completion_clock ON profiled_auto_settlements(updated_at_ms)
    WHERE disposition = 'confirmed';
CREATE INDEX performance_pilot_completion_clock ON performance_pilot_completion_times(confirmed_at_ms);
CREATE INDEX performance_merge_clock ON delivery_merge_evidence(observed_at_ms);
CREATE INDEX performance_merge_task ON delivery_merge_evidence(project_id,task_id,observed_at_ms);
CREATE INDEX performance_delivery_decision_clock ON delivery_decisions(decided_at_ms);
CREATE INDEX performance_result_task ON result_revision_pins(project_id,task_id,submitted_at_ms,report_id);
CREATE INDEX performance_cancelled_clock ON profiled_cancelled_settlements(recorded_at_ms);

-- Optional scheduling diagnostics. Actual claim ownership remains in execution_attempts.
CREATE TABLE performance_scheduling_minute (
    minute_ms BIGINT NOT NULL,
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 96),
    count BIGINT NOT NULL CHECK (count > 0),
    PRIMARY KEY (minute_ms, reason)
);

CREATE TABLE performance_scheduling_decisions (
    operation_id UUID NOT NULL,
    task_id UUID NOT NULL,
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 96),
    observed_at_ms BIGINT NOT NULL,
    PRIMARY KEY (operation_id, task_id, reason)
);
CREATE INDEX performance_scheduling_decisions_age ON performance_scheduling_decisions (observed_at_ms);

CREATE TABLE performance_scheduling_examples (
    minute_ms BIGINT NOT NULL,
    example JSONB NOT NULL CHECK (jsonb_array_length(example) <= 8),
    PRIMARY KEY (minute_ms)
);

CREATE TABLE performance_scheduling_timings (
    attempt_id UUID PRIMARY KEY,
    task_id UUID NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    eligible_at_ms BIGINT NOT NULL,
    claimed_at_ms BIGINT NOT NULL CHECK (claimed_at_ms >= eligible_at_ms)
);
CREATE INDEX performance_scheduling_timings_age ON performance_scheduling_timings (claimed_at_ms);

CREATE INDEX performance_scheduling_native_claim_at ON profiled_launches (
    ((graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT)
);

CREATE INDEX performance_observation_facts_call ON observation_facts(call_key, event_id);
CREATE INDEX performance_observation_task_window ON observation_events(attempt_id, recorded_at_ms)
    WHERE decision->>'kind' IN ('append', 'unresolved') OR record#>>'{body,kind,kind}' = 'gap';

-- Derive expected attempts from verified starts, including starts that yielded
-- no observation events.
CREATE FUNCTION performance_started_at(receipt BYTEA) RETURNS BIGINT
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
RETURN (convert_from(receipt, 'UTF8')::jsonb->>'recorded_at_ms')::bigint;
CREATE INDEX performance_observation_started_at ON profiled_started_receipts
    (performance_started_at(receipt_bytes));
CREATE INDEX performance_internal_usage_window ON internal_tasks(finished_at_ms)
    WHERE status IN ('answered', 'failed') AND affected_kind <> 'task';

UPDATE meta SET value='178' WHERE key='state_format_version';
