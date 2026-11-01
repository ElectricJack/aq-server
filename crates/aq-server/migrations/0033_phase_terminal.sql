-- Typed phase terminal origin and retained cleanup capacity. No route activation.
-- Refuse unknown terminal histories; do not relabel an older writer's claim.
DO $$ BEGIN
 IF EXISTS(SELECT 1 FROM profiled_started_receipts r JOIN profiled_v2_accepts a USING(attempt_id))
 OR EXISTS(SELECT 1 FROM profiled_stopped_receipts r JOIN profiled_v2_accepts a USING(attempt_id))
 OR EXISTS(SELECT 1 FROM profiled_launch_closures r JOIN profiled_v2_accepts a USING(attempt_id))
 THEN RAISE EXCEPTION 'unqualified existing phase terminal history requires explicit recovery'; END IF;
END $$;
CREATE TABLE profiled_phase_terminal_capacity (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_phase_execution_grants(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 reserved_bytes BIGINT NOT NULL DEFAULT 1048576 CHECK(reserved_bytes=1048576)
);
CREATE INDEX profiled_phase_terminal_capacity_node ON profiled_phase_terminal_capacity(node_id);
CREATE FUNCTION profiled_phase_terminal_capacity_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n BIGINT; b BIGINT; pn BIGINT; pb BIGINT;
BEGIN
 IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'retained phase terminal capacity'; END IF;
 IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'phase capacity requires current predicate visibility'; END IF;
 IF NOT EXISTS(SELECT 1 FROM profiled_phase_execution_grants g WHERE g.attempt_id=NEW.attempt_id AND g.node_id=NEW.node_id) THEN RAISE EXCEPTION 'actual phase grant capacity owner required'; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-phase-terminal-capacity-v1',0));
 SELECT count(*),COALESCE(sum(reserved_bytes),0),count(*) FILTER(WHERE node_id=NEW.node_id),COALESCE(sum(reserved_bytes) FILTER(WHERE node_id=NEW.node_id),0) INTO n,b,pn,pb FROM profiled_phase_terminal_capacity;
 IF n>=256 OR b+NEW.reserved_bytes>268435456 OR pn>=16 OR pb+NEW.reserved_bytes>16777216 THEN RAISE EXCEPTION 'phase terminal capacity exhausted before grant issuance'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_phase_terminal_capacity_guard BEFORE INSERT OR UPDATE OR DELETE ON profiled_phase_terminal_capacity FOR EACH ROW EXECUTE FUNCTION profiled_phase_terminal_capacity_guard();
INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT attempt_id,node_id FROM profiled_phase_execution_grants ORDER BY attempt_id;
CREATE FUNCTION profiled_phase_terminal_reserve() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) VALUES(NEW.attempt_id,NEW.node_id); RETURN NULL; END $$;
CREATE TRIGGER profiled_phase_terminal_reserve AFTER INSERT ON profiled_phase_execution_grants FOR EACH ROW EXECUTE FUNCTION profiled_phase_terminal_reserve();
ALTER TABLE profiled_started_receipts
 ADD COLUMN initial_authority_kind TEXT NOT NULL DEFAULT 'legacy_prepare_v1' CHECK(initial_authority_kind IN ('legacy_prepare_v1','phase_execution_v2')),
 ADD COLUMN initial_authority_receipt_digest TEXT CHECK(initial_authority_receipt_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_request_digest TEXT CHECK(phase_request_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_envelope_bytes BYTEA CHECK(octet_length(phase_envelope_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_ack_bytes BYTEA CHECK(octet_length(phase_ack_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_envelope_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_envelope_bytes),'hex')) STORED,
 ADD COLUMN phase_ack_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_ack_bytes),'hex')) STORED,
 ADD COLUMN phase_terminal_qualification JSONB CHECK(octet_length(phase_terminal_qualification::text) BETWEEN 1 AND 4096),
 ADD CONSTRAINT profiled_started_receipts_phase_origin_check CHECK(
 (initial_authority_kind='legacy_prepare_v1' AND initial_authority_receipt_digest IS NULL AND phase_request_digest IS NULL AND phase_envelope_bytes IS NULL AND phase_ack_bytes IS NULL AND phase_terminal_qualification IS NULL)
 OR (initial_authority_kind='phase_execution_v2' AND initial_authority_receipt_digest IS NOT NULL AND phase_request_digest IS NOT NULL AND phase_envelope_bytes IS NOT NULL AND phase_ack_bytes IS NOT NULL AND phase_terminal_qualification IS NOT NULL));
ALTER TABLE profiled_stopped_receipts
 ADD COLUMN initial_authority_kind TEXT NOT NULL DEFAULT 'legacy_prepare_v1' CHECK(initial_authority_kind IN ('legacy_prepare_v1','phase_execution_v2')),
 ADD COLUMN initial_authority_receipt_digest TEXT CHECK(initial_authority_receipt_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_request_digest TEXT CHECK(phase_request_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_envelope_bytes BYTEA CHECK(octet_length(phase_envelope_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_ack_bytes BYTEA CHECK(octet_length(phase_ack_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_envelope_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_envelope_bytes),'hex')) STORED,
 ADD COLUMN phase_ack_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_ack_bytes),'hex')) STORED,
 ADD COLUMN phase_terminal_qualification JSONB CHECK(octet_length(phase_terminal_qualification::text) BETWEEN 1 AND 4096),
 ADD CONSTRAINT profiled_stopped_receipts_phase_origin_check CHECK(
 (initial_authority_kind='legacy_prepare_v1' AND initial_authority_receipt_digest IS NULL AND phase_request_digest IS NULL AND phase_envelope_bytes IS NULL AND phase_ack_bytes IS NULL AND phase_terminal_qualification IS NULL)
 OR (initial_authority_kind='phase_execution_v2' AND initial_authority_receipt_digest IS NOT NULL AND phase_request_digest IS NOT NULL AND phase_envelope_bytes IS NOT NULL AND phase_ack_bytes IS NOT NULL AND phase_terminal_qualification IS NOT NULL));
ALTER TABLE profiled_launch_closures
 ADD COLUMN initial_authority_kind TEXT NOT NULL DEFAULT 'legacy_prepare_v1' CHECK(initial_authority_kind IN ('legacy_prepare_v1','phase_execution_v2')),
 ADD COLUMN initial_authority_receipt_digest TEXT CHECK(initial_authority_receipt_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_request_digest TEXT CHECK(phase_request_digest ~ '^[0-9a-f]{64}$'),
 ADD COLUMN phase_envelope_bytes BYTEA CHECK(octet_length(phase_envelope_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_ack_bytes BYTEA CHECK(octet_length(phase_ack_bytes) BETWEEN 1 AND 65536),
 ADD COLUMN phase_envelope_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_envelope_bytes),'hex')) STORED,
 ADD COLUMN phase_ack_digest TEXT GENERATED ALWAYS AS (encode(sha256(phase_ack_bytes),'hex')) STORED,
 ADD COLUMN phase_terminal_qualification JSONB CHECK(octet_length(phase_terminal_qualification::text) BETWEEN 1 AND 4096),
 ADD CONSTRAINT profiled_launch_closures_phase_origin_check CHECK(
 (initial_authority_kind='legacy_prepare_v1' AND initial_authority_receipt_digest IS NULL AND phase_request_digest IS NULL AND phase_envelope_bytes IS NULL AND phase_ack_bytes IS NULL AND phase_terminal_qualification IS NULL)
 OR (initial_authority_kind='phase_execution_v2' AND initial_authority_receipt_digest IS NOT NULL AND phase_request_digest IS NOT NULL AND phase_envelope_bytes IS NOT NULL AND phase_ack_bytes IS NOT NULL AND phase_terminal_qualification IS NOT NULL));
-- Actual variable retained bytes; fixed row/index/WAL overhead is not a disk claim.
CREATE FUNCTION profiled_phase_terminal_extra_bytes(kind TEXT,origin TEXT,request_digest TEXT,envelope BYTEA,ack BYTEA,qualification JSONB) RETURNS BIGINT LANGUAGE SQL IMMUTABLE AS $$
 SELECT CASE WHEN kind='phase_execution_v2' THEN octet_length(kind)::BIGINT+octet_length(origin)+octet_length(request_digest)+octet_length(envelope)+octet_length(ack)+octet_length(qualification::TEXT)+128 ELSE 0::BIGINT END
$$;
CREATE FUNCTION profiled_phase_terminal_used_bytes(attempt UUID) RETURNS BIGINT LANGUAGE SQL VOLATILE AS $$
 SELECT COALESCE((SELECT sum(octet_length(request_bytes)::BIGINT+octet_length(receipt_bytes)+64+profiled_phase_terminal_extra_bytes(initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification)) FROM profiled_started_receipts WHERE attempt_id=attempt AND initial_authority_kind='phase_execution_v2'),0)
 +COALESCE((SELECT sum(octet_length(request_bytes)::BIGINT+octet_length(proof_bytes)+octet_length(receipt_bytes)+128+profiled_phase_terminal_extra_bytes(initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification)) FROM profiled_stopped_receipts WHERE attempt_id=attempt AND initial_authority_kind='phase_execution_v2'),0)
 +COALESCE((SELECT sum(octet_length(request_bytes)::BIGINT+octet_length(context_bytes)+octet_length(receipt_bytes)+octet_length(binding::TEXT)+octet_length(verifier::TEXT)+256+profiled_phase_terminal_extra_bytes(initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,phase_terminal_qualification)) FROM profiled_launch_closures WHERE attempt_id=attempt AND initial_authority_kind='phase_execution_v2'),0)
$$;
CREATE FUNCTION profiled_phase_terminal_origin_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE phase BOOLEAN; g profiled_phase_execution_grants%ROWTYPE; q JSONB; r JSONB; envelope JSONB; ack JSONB; incoming BIGINT; budget BIGINT; stopped_origin TEXT;
BEGIN
 phase:=EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE attempt_id=NEW.attempt_id) OR EXISTS(SELECT 1 FROM profiled_phase_execution_grants WHERE attempt_id=NEW.attempt_id);
 IF NOT phase THEN
  IF NEW.initial_authority_kind<>'legacy_prepare_v1' THEN RAISE EXCEPTION 'phase origin requires actual phase ownership'; END IF;
  RETURN NEW;
 END IF;
 SELECT * INTO STRICT g FROM profiled_phase_execution_grants WHERE attempt_id=NEW.attempt_id;
 IF NEW.initial_authority_kind<>'phase_execution_v2' OR NEW.node_id<>g.node_id OR NEW.initial_authority_receipt_digest IS DISTINCT FROM g.receipt_digest
 OR current_setting('aq.phase_terminal_receipt',true) IS DISTINCT FROM g.receipt_digest THEN RAISE EXCEPTION 'qualified transaction-local phase terminal writer required'; END IF;
 IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'phase terminal requires current predicate visibility'; END IF;
 q:=convert_from(NEW.request_bytes,'UTF8')::JSONB; r:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 envelope:=convert_from(NEW.phase_envelope_bytes,'UTF8')::JSONB; ack:=convert_from(NEW.phase_ack_bytes,'UTF8')::JSONB;
 IF envelope->>'schema_version' IS DISTINCT FROM '1' OR ack->>'schema_version' IS DISTINCT FROM '1'
 OR envelope#>>'{negotiation,protocol}' IS DISTINCT FROM 'aq.phase-terminal.v1' OR ack->'negotiation' IS DISTINCT FROM envelope->'negotiation'
 OR envelope#>>'{origin,kind}' IS DISTINCT FROM 'phase_execution_v2' OR envelope#>>'{origin,execution_receipt_digest}' IS DISTINCT FROM g.receipt_digest
 OR envelope#>>'{origin,accepted_receipt_digest}' IS DISTINCT FROM g.accepted_receipt_digest OR ack->'origin' IS DISTINCT FROM envelope->'origin'
 OR envelope->'original' IS DISTINCT FROM q OR ack->'original' IS DISTINCT FROM r OR ack->>'request_digest' IS DISTINCT FROM NEW.phase_request_digest
 OR NEW.phase_terminal_qualification->>'id' IS NULL OR NEW.phase_terminal_qualification->>'version' IS NULL OR COALESCE(NEW.phase_terminal_qualification->>'digest','')!~'^[0-9a-f]{64}$'
 THEN RAISE EXCEPTION 'exact phase terminal envelope/ack origin required'; END IF;
 IF TG_TABLE_NAME='profiled_stopped_receipts' THEN
  SELECT initial_authority_receipt_digest INTO STRICT stopped_origin FROM profiled_started_receipts WHERE attempt_id=NEW.attempt_id AND initial_authority_kind='phase_execution_v2';
  IF stopped_origin IS DISTINCT FROM g.receipt_digest THEN RAISE EXCEPTION 'phase stopped requires actual matching phase Started'; END IF;
 END IF;
 incoming:=octet_length(NEW.request_bytes)::BIGINT+octet_length(NEW.receipt_bytes)+profiled_phase_terminal_extra_bytes(NEW.initial_authority_kind,NEW.initial_authority_receipt_digest,NEW.phase_request_digest,NEW.phase_envelope_bytes,NEW.phase_ack_bytes,NEW.phase_terminal_qualification);
 IF TG_TABLE_NAME='profiled_started_receipts' THEN incoming:=incoming+64; END IF;
 IF TG_TABLE_NAME='profiled_stopped_receipts' THEN incoming:=incoming+octet_length(NEW.proof_bytes)+128; END IF;
 IF TG_TABLE_NAME='profiled_launch_closures' THEN incoming:=incoming+octet_length(NEW.context_bytes)+octet_length(NEW.binding::TEXT)+octet_length(NEW.verifier::TEXT)+256; END IF;
 SELECT reserved_bytes INTO STRICT budget FROM profiled_phase_terminal_capacity WHERE attempt_id=NEW.attempt_id AND node_id=NEW.node_id FOR UPDATE;
 IF profiled_phase_terminal_used_bytes(NEW.attempt_id)+incoming>budget THEN RAISE EXCEPTION 'phase terminal actual bytes exceed retained reservation'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_phase_terminal_origin_guard BEFORE INSERT ON profiled_started_receipts FOR EACH ROW EXECUTE FUNCTION profiled_phase_terminal_origin_guard();
CREATE TRIGGER profiled_phase_terminal_origin_guard BEFORE INSERT ON profiled_stopped_receipts FOR EACH ROW EXECUTE FUNCTION profiled_phase_terminal_origin_guard();
CREATE TRIGGER profiled_phase_terminal_origin_guard BEFORE INSERT ON profiled_launch_closures FOR EACH ROW EXECUTE FUNCTION profiled_phase_terminal_origin_guard();
-- Preserve real parent FK delete/key-update semantics in both origin namespaces.
-- Generated NULL keys select exactly the actual parent, never a facade row.
DO $$ DECLARE c TEXT; n BIGINT; BEGIN
 SELECT count(*),min(conname) INTO n,c FROM pg_constraint WHERE conrelid='profiled_launch_closures'::regclass AND confrelid='profiled_execution_receipts'::regclass AND contype='f';
 IF n<>1 THEN RAISE EXCEPTION 'exact legacy closure origin FK required'; END IF;
 EXECUTE format('ALTER TABLE profiled_launch_closures DROP CONSTRAINT %I',c);
END $$;
ALTER TABLE profiled_launch_closures
 ADD COLUMN legacy_origin_operation_id UUID GENERATED ALWAYS AS (CASE WHEN initial_authority_kind='legacy_prepare_v1' THEN initial_prepare_operation_id END) STORED,
 ADD COLUMN phase_origin_operation_id UUID GENERATED ALWAYS AS (CASE WHEN initial_authority_kind='phase_execution_v2' THEN initial_prepare_operation_id END) STORED,
 ADD CONSTRAINT profiled_launch_closures_legacy_origin_fk FOREIGN KEY(node_id,legacy_origin_operation_id) REFERENCES profiled_execution_receipts(node_id,operation_id),
 ADD CONSTRAINT profiled_launch_closures_phase_origin_fk FOREIGN KEY(node_id,phase_origin_operation_id) REFERENCES profiled_phase_execution_grants(node_id,operation_id);
CREATE OR REPLACE FUNCTION profiled_launch_closure_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE a execution_attempts%ROWTYPE; h profiled_authority_hot%ROWTYPE; history profiled_native_registration_history%ROWTYPE; native JSONB; original JSONB; req JSONB; receipt JSONB; n BIGINT; b BIGINT; per_n BIGINT; per_b BIGINT; incoming BIGINT; phase BOOLEAN;
BEGIN
 SELECT * INTO STRICT a FROM execution_attempts WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO STRICT h FROM profiled_authority_hot WHERE attempt_id=NEW.attempt_id;
 phase:=EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE attempt_id=NEW.attempt_id) OR EXISTS(SELECT 1 FROM profiled_phase_execution_grants WHERE attempt_id=NEW.attempt_id);
 IF phase THEN
  SELECT convert_from(receipt_bytes,'UTF8')::JSONB INTO STRICT original FROM profiled_phase_execution_grants WHERE node_id=NEW.node_id AND attempt_id=NEW.attempt_id AND operation_id=NEW.initial_prepare_operation_id;
 ELSE
  SELECT response INTO STRICT original FROM profiled_execution_receipts WHERE node_id=NEW.node_id AND operation_id=NEW.initial_prepare_operation_id AND kind='prepare';
 END IF;
 SELECT * INTO STRICT history FROM profiled_native_registration_history WHERE registration_id=NEW.registration_id; native:=convert_from(history.metadata_bytes,'UTF8')::JSONB;
 req:=convert_from(NEW.request_bytes,'UTF8')::JSONB; receipt:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 IF a.node_id<>NEW.node_id OR NOT a.holds_task OR a.kind<>'profiled_v1' OR EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=NEW.attempt_id)
 OR h.state#>>'{stop,kind}' IS DISTINCT FROM 'unknown' OR h.state->>'revoked' IS DISTINCT FROM 'true' OR h.state->>'status'='granted' OR h.state#>>'{grant,grant_id}' IS DISTINCT FROM NEW.grant_id::TEXT OR h.state#>'{grant,binding}' IS DISTINCT FROM NEW.binding
 OR (phase AND (NEW.initial_authority_kind<>'phase_execution_v2' OR original#>'{grant,binding}' IS DISTINCT FROM NEW.binding OR original#>>'{grant,grant_id}' IS DISTINCT FROM NEW.grant_id::TEXT))
 OR (NOT phase AND (NEW.initial_authority_kind<>'legacy_prepare_v1' OR original->>'kind' IS DISTINCT FROM 'grant' OR original#>'{body,grant,binding}' IS DISTINCT FROM NEW.binding OR original#>>'{body,grant,grant_id}' IS DISTINCT FROM NEW.grant_id::TEXT))
 OR NOT EXISTS(SELECT 1 FROM profiled_launch_registrations WHERE attempt_id=NEW.attempt_id AND registration_id=NEW.registration_id AND node_id=NEW.node_id)
 OR history.node_id<>NEW.node_id OR history.boot_id::TEXT IS DISTINCT FROM NEW.binding#>>'{session,boot_id}' OR native#>'{request,runtime}' IS DISTINCT FROM req->'runtime' OR native->'qualification' IS DISTINCT FROM req->'qualification' OR NEW.verifier->'runtime' IS DISTINCT FROM req->'runtime' OR NEW.verifier->'qualification' IS DISTINCT FROM req->'qualification' OR (NEW.verifier->'kinds' ? NEW.evidence_kind) IS DISTINCT FROM true
 OR req->'binding' IS DISTINCT FROM NEW.binding OR req->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT OR req->>'grant_id' IS DISTINCT FROM NEW.grant_id::TEXT OR req#>>'{journal,revision}' IS DISTINCT FROM NEW.terminal_revision::TEXT OR req#>>'{evidence,kind}' IS DISTINCT FROM NEW.evidence_kind
 OR receipt->'binding' IS DISTINCT FROM NEW.binding OR receipt->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT OR receipt->>'grant_id' IS DISTINCT FROM NEW.grant_id::TEXT OR receipt->>'request_digest' IS DISTINCT FROM NEW.request_digest OR receipt->>'disposition' IS DISTINCT FROM 'launch_closed_awaiting_settlement' OR receipt->>'recorded_at_ms' IS DISTINCT FROM NEW.recorded_at_ms::TEXT OR receipt->>'authority_revision_at_record' IS DISTINCT FROM NEW.authority_revision::TEXT
 THEN RAISE EXCEPTION 'exact granted closure owner required'; END IF;
 IF current_setting('transaction_isolation')<>'serializable' THEN RAISE EXCEPTION 'closure mutation isolation'; END IF;
 IF phase THEN RETURN NEW; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-profiled-launch-closure-quota-v1',0));
 SELECT count(*),COALESCE(sum(octet_length(request_bytes)+octet_length(context_bytes)+octet_length(receipt_bytes)+octet_length(binding::text)+octet_length(verifier::text)),0),count(*) FILTER(WHERE node_id=NEW.node_id),COALESCE(sum(octet_length(request_bytes)+octet_length(context_bytes)+octet_length(receipt_bytes)+octet_length(binding::text)+octet_length(verifier::text)) FILTER(WHERE node_id=NEW.node_id),0) INTO n,b,per_n,per_b FROM profiled_launch_closures WHERE initial_authority_kind='legacy_prepare_v1';
 incoming:=octet_length(NEW.request_bytes)+octet_length(NEW.context_bytes)+octet_length(NEW.receipt_bytes)+octet_length(NEW.binding::text)+octet_length(NEW.verifier::text);
 IF n>=4096 OR b+incoming>67108864 OR per_n>=256 OR per_b+incoming>4194304 THEN RAISE EXCEPTION 'launch closure history quota'; END IF;
 RETURN NEW;
END $$;
-- Terminal evidence does not qualify legacy result recording or settlement.
-- Old already-open writers must not acquire these capabilities via generic Stop.
CREATE FUNCTION profiled_phase_unqualified_result_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE attempt_id=NEW.attempt_id)
 OR EXISTS(SELECT 1 FROM profiled_phase_execution_grants WHERE attempt_id=NEW.attempt_id)
 THEN RAISE EXCEPTION 'phase result and settlement protocol not qualified' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_phase_unqualified_result_guard BEFORE INSERT ON profiled_result_reports FOR EACH ROW EXECUTE FUNCTION profiled_phase_unqualified_result_guard();
CREATE TRIGGER profiled_phase_unqualified_release_guard BEFORE INSERT ON profiled_verified_releases FOR EACH ROW EXECUTE FUNCTION profiled_phase_unqualified_result_guard();
UPDATE meta SET value='33' WHERE key='state_format_version';
