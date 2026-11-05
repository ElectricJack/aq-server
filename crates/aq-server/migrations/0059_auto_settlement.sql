-- D3: automatic default-policy settlement and the graph-event outbox consumer.
-- Server-owned check evidence (seam for D6). No worker or agent route writes it.
-- Evidence binds the exact retained source (import operation, checkpoint OID)
-- and the exact required-check validator digest.
CREATE TABLE profiled_check_evidence (
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 check_id TEXT NOT NULL CHECK(length(check_id) BETWEEN 1 AND 256),
 source_operation_id UUID NOT NULL,
 checkpoint TEXT NOT NULL CHECK(checkpoint ~ '^[0-9a-f]{40}$'),
 validator_digest TEXT NOT NULL CHECK(validator_digest ~ '^[0-9a-f]{64}$'),
 verdict TEXT NOT NULL CHECK(verdict IN ('passed','failed')),
 evidence JSONB NOT NULL CHECK(octet_length(evidence::text)<=16384),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms>0),
 PRIMARY KEY(attempt_id,check_id,source_operation_id)
);
-- Server-owned publication evidence (seam for D1a/D1b delivery owners).
CREATE TABLE profiled_delivery_evidence (
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 source_operation_id UUID NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('published','unpublished','indeterminate')),
 evidence JSONB NOT NULL CHECK(octet_length(evidence::text)<=16384),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms>0),
 PRIMARY KEY(attempt_id,source_operation_id)
);
-- One policy-actor settlement record per recorded report. A refusal is durable
-- rework evidence; only a validated Graph receipt makes it 'confirmed'.
CREATE TABLE profiled_auto_settlements (
 report_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 policy_actor TEXT NOT NULL CHECK(policy_actor='aq.settlement.default-policy@1'),
 disposition TEXT NOT NULL CHECK(disposition IN ('held','rework_required','prepared','possible','confirmed')),
 reason TEXT CHECK(reason IS NULL OR length(reason) BETWEEN 1 AND 64),
 evidence JSONB NOT NULL CHECK(octet_length(evidence::text)<=65536),
 operation_id UUID UNIQUE,
 workspace_id UUID,
 task_id UUID,
 expected_version BIGINT CHECK(expected_version IS NULL OR expected_version>=0),
 expected_content_revision BIGINT CHECK(expected_content_revision IS NULL OR expected_content_revision>=0),
 payload_bytes BYTEA CHECK(payload_bytes IS NULL OR octet_length(payload_bytes)<=8192),
 receipt JSONB,
 dispatch_count INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_count BETWEEN 0 AND 8),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>0),
 CHECK(disposition NOT IN ('prepared','possible','confirmed') OR (operation_id IS NOT NULL AND workspace_id IS NOT NULL AND task_id IS NOT NULL AND expected_version IS NOT NULL AND expected_content_revision IS NOT NULL AND payload_bytes IS NOT NULL)),
 CHECK((disposition='confirmed')=(receipt IS NOT NULL))
);
CREATE INDEX profiled_auto_settlement_attempt ON profiled_auto_settlements(attempt_id);
CREATE FUNCTION profiled_auto_settlement_owner() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF current_setting('aq.settlement_owner',true) IS DISTINCT FROM 'aq.settlement-owner.auto.1' THEN RAISE EXCEPTION 'automatic settlement owner required'; END IF;
 IF TG_OP='UPDATE' AND (OLD.disposition='confirmed'
   OR ROW(NEW.report_id,NEW.attempt_id,NEW.created_at_ms) IS DISTINCT FROM ROW(OLD.report_id,OLD.attempt_id,OLD.created_at_ms)
   OR (OLD.operation_id IS NOT NULL AND ROW(NEW.operation_id,NEW.expected_version,NEW.payload_bytes) IS DISTINCT FROM ROW(OLD.operation_id,OLD.expected_version,OLD.payload_bytes)))
 THEN RAISE EXCEPTION 'immutable automatic settlement'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_auto_settlement_owner BEFORE INSERT OR UPDATE OR DELETE ON profiled_auto_settlements
 FOR EACH ROW EXECUTE FUNCTION profiled_auto_settlement_owner();
-- A confirmed automatic completion receipt is an additional release proof,
-- alongside format 52's replacement release.
CREATE OR REPLACE FUNCTION settlement_claim_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 AND NOT profiled_never_granted_valid(OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_task_completions c JOIN profiled_settlement_evaluations e ON e.operation_id=c.evaluation_operation_id JOIN profiled_verified_releases r ON r.attempt_id=c.attempt_id AND r.release_id=c.release_id WHERE c.attempt_id=OLD.attempt_id AND c.state='confirmed' AND e.disposition='accepted' AND e.operation_id=r.settlement_operation_id AND c.receipt IS NOT NULL AND c.outbox IS NOT NULL)
 AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.attempt_id=OLD.attempt_id AND a.disposition='confirmed' AND a.receipt IS NOT NULL)
 THEN RAISE EXCEPTION 'claim remains held until actual Graph completion receipt'; END IF;
 RETURN NEW;
END $$;
-- The same confirmed automatic receipt is the owner-verified release record.
CREATE OR REPLACE FUNCTION profiled_historical_snapshot_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND (NEW.state IS DISTINCT FROM OLD.state OR NEW.execution_deadline_ms IS DISTINCT FROM OLD.execution_deadline_ms)
 THEN RAISE EXCEPTION 'hot authority owns current profiled state' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND NOT OLD.holds_task AND NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 THEN RAISE EXCEPTION 'released profiled claim cannot be reacquired' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_verified_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.attempt_id=OLD.attempt_id AND a.disposition='confirmed' AND a.receipt IS NOT NULL)
    AND NOT profiled_never_granted_valid(OLD.attempt_id)
 THEN RAISE EXCEPTION 'actual owner-verified release record required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
-- Graph-event producer and the catalog profile that trusts it.
ALTER TABLE playbook_outbox DROP CONSTRAINT playbook_outbox_producer_check;
ALTER TABLE playbook_outbox ADD CHECK(producer IN ('aq.host.bridge@1','aq.human.decisions@1','aq.graph.operations@1'));
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3));
UPDATE meta SET value='59' WHERE key='state_format_version';
