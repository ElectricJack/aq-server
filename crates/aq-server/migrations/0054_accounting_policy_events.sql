-- H2: lineage budget policy events and the attempt-usage owner index.
-- A soft ceiling is crossed at most once per (lineage, ceiling, threshold,
-- currency); the event is immutable evidence, not enforcement.
CREATE TABLE accounting_policy_events (
 lineage_id UUID NOT NULL,
 ceiling TEXT NOT NULL CHECK(ceiling IN ('project_soft_cost')),
 threshold_microunits BIGINT NOT NULL CHECK(threshold_microunits >= 0),
 spent_microunits BIGINT NOT NULL CHECK(spent_microunits >= threshold_microunits),
 currency TEXT NOT NULL CHECK(currency ~ '^[A-Z]{3}$'),
 ledger_revision BIGINT NOT NULL CHECK(ledger_revision >= 0),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms > 0),
 PRIMARY KEY(lineage_id, ceiling, threshold_microunits, currency)
);
CREATE TRIGGER accounting_policy_event_immutable BEFORE UPDATE OR DELETE ON accounting_policy_events
 FOR EACH ROW EXECUTE FUNCTION refuse_observation_history_mutation();
CREATE INDEX observation_call_heads_owner ON observation_call_heads(owner_attempt_id)
 WHERE owner_attempt_id IS NOT NULL;
UPDATE meta SET value='54' WHERE key='state_format_version';
