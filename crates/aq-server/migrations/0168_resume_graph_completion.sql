-- An unresolved Graph response leaves the original operation live. Every
-- reconciliation round has a durable check time and the same operation key.
ALTER TABLE profiled_auto_settlements
 DROP CONSTRAINT profiled_auto_settlements_dispatch_count_check,
 ADD CONSTRAINT profiled_auto_settlements_dispatch_count_check CHECK(dispatch_count >= 0),
 ADD COLUMN graph_service JSONB,
 ADD COLUMN next_graph_check_at_ms BIGINT;

CREATE INDEX profiled_auto_settlements_due_graph_check
 ON profiled_auto_settlements(report_id, next_graph_check_at_ms)
 WHERE disposition IN ('prepared','possible') AND evidence->>'decided_by' IS DISTINCT FROM 'playbook';

CREATE OR REPLACE FUNCTION profiled_auto_settlement_owner() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF current_setting('aq.settlement_owner',true) IS DISTINCT FROM 'aq.settlement-owner.auto.1' THEN RAISE EXCEPTION 'automatic settlement owner required'; END IF;
 IF TG_OP='UPDATE' AND (OLD.disposition='confirmed'
   OR ROW(NEW.report_id,NEW.attempt_id,NEW.created_at_ms) IS DISTINCT FROM ROW(OLD.report_id,OLD.attempt_id,OLD.created_at_ms)
   OR (OLD.operation_id IS NOT NULL AND ROW(NEW.operation_id,NEW.expected_version,NEW.payload_bytes,NEW.graph_service) IS DISTINCT FROM ROW(OLD.operation_id,OLD.expected_version,OLD.payload_bytes,OLD.graph_service))
   OR NEW.dispatch_count < OLD.dispatch_count)
 THEN RAISE EXCEPTION 'immutable automatic settlement'; END IF;
 RETURN NEW;
END $$;

UPDATE meta SET value='168' WHERE key='state_format_version';
