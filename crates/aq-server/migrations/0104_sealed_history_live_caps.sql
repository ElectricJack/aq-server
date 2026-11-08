-- Spec 0008 phase A: these quota triggers count live rows only (sealed_history_usage, 0103).
CREATE OR REPLACE FUNCTION admission_artifact_insert_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE total_count BIGINT; total_bytes BIGINT; project_count BIGINT; project_bytes BIGINT;
BEGIN
 IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'artifact quota requires current predicate visibility'; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-inline-artifact-quota-v1',0));
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO total_count,total_bytes,project_count,project_bytes
 FROM sealed_history_usage('admission_artifacts',CASE WHEN NEW.owner_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' THEN NEW.owner_key::UUID END,NEW.created_at_ms) u;
 IF total_count>=4096 OR project_count>=512 OR total_bytes+COALESCE(octet_length(NEW.body),0)>268435456
    OR project_bytes+COALESCE(octet_length(NEW.body),0)>67108864 THEN RAISE EXCEPTION 'admission artifact quota'; END IF;
 IF NEW.body IS NULL AND (NEW.owner_kind<>'source_retention' OR NEW.descriptor->>'kind' IS DISTINCT FROM 'source') THEN RAISE EXCEPTION 'non-source artifact requires inline bytes'; END IF;
 IF NEW.body IS NOT NULL AND (
    NEW.byte_length<>octet_length(NEW.body) OR NEW.content_digest<>encode(sha256(NEW.body),'hex')
    OR NEW.descriptor->'artifact'->>'digest' IS DISTINCT FROM NEW.content_digest
    OR (NEW.descriptor->>'bytes')::BIGINT IS DISTINCT FROM NEW.byte_length
    OR (NEW.descriptor->>'retained_at_ms')::BIGINT IS DISTINCT FROM NEW.created_at_ms
    OR (NEW.descriptor->>'retain_until_ms')::BIGINT IS DISTINCT FROM NEW.retain_until_ms
    OR NEW.descriptor->>'kind' IS NULL
    OR NEW.descriptor->>'kind' NOT IN ('context','composition','selection','definition','outcome','source')
    OR NEW.owner_key !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
 ) THEN RAISE EXCEPTION 'inline artifact byte/descriptor mismatch'; END IF;
 IF NEW.body IS NOT NULL AND NEW.descriptor->>'kind'='source' AND (
    NEW.owner_kind<>'source_retention'
    OR NEW.descriptor#>>'{artifact,id}' IS DISTINCT FROM 'aq.source-admission-observation/'||NEW.content_digest
    OR NEW.descriptor#>>'{artifact,version}' IS DISTINCT FROM '1'
    OR convert_from(NEW.body,'UTF8')::JSONB->>'schema_version' IS DISTINCT FROM '1'
    OR convert_from(NEW.body,'UTF8')::JSONB#>>'{admission,schema_version}' IS DISTINCT FROM '1'
    OR convert_from(NEW.body,'UTF8')::JSONB#>>'{admission,project_id}' IS DISTINCT FROM NEW.owner_key
 ) THEN RAISE EXCEPTION 'typed source admission owner mismatch'; END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION profiled_source_export_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE q JSONB; v JSONB; n BIGINT; pn BIGINT; total_metadata BIGINT; project_metadata BIGINT; incoming_metadata BIGINT;
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'source export owner cannot be deleted'; END IF;
 q:=convert_from(NEW.request_bytes,'UTF8')::JSONB;
 IF q->>'schema_version' IS DISTINCT FROM '1' OR q->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT
    OR q->>'project_id' IS DISTINCT FROM NEW.project_id::TEXT
 THEN RAISE EXCEPTION 'source export request owner mismatch'; END IF;
 PERFORM pg_advisory_xact_lock(719247031);
 incoming_metadata:=octet_length(NEW.request_bytes)+COALESCE(octet_length(NEW.verified_bytes),4194304);
 SELECT u.total_bytes,u.scope_bytes INTO total_metadata,project_metadata
 FROM sealed_history_usage('source_exports',NEW.project_id,0,NEW.operation_id) u;
 IF total_metadata+incoming_metadata>268435456 OR project_metadata+incoming_metadata>67108864
 THEN RAISE EXCEPTION 'source export metadata quota'; END IF;
 IF TG_OP='INSERT' THEN
  IF NEW.state<>'pending' OR NEW.charged_bytes<>NEW.reserved_bytes THEN RAISE EXCEPTION 'source export needs prior reservation'; END IF;
  IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'source export quota needs current visibility'; END IF;
  SELECT u.total_rows,u.scope_rows INTO n,pn FROM sealed_history_usage('source_exports',NEW.project_id,0) u;
  IF n>=4096 OR pn>=256
  THEN RAISE EXCEPTION 'source export row quota'; END IF;
 ELSE
  IF ROW(NEW.operation_id,NEW.project_id,NEW.request_bytes,NEW.request_digest,NEW.reserved_bytes)
      IS DISTINCT FROM ROW(OLD.operation_id,OLD.project_id,OLD.request_bytes,OLD.request_digest,OLD.reserved_bytes)
    OR (OLD.state='verified' AND NEW IS DISTINCT FROM OLD)
    OR (OLD.state='pending' AND NEW.state NOT IN ('pending','verified'))
    OR (NEW.state='pending' AND NEW.charged_bytes<>OLD.charged_bytes)
  THEN RAISE EXCEPTION 'source export identity/completion is immutable'; END IF;
 END IF;
 IF NEW.state='verified' THEN
  v:=convert_from(NEW.verified_bytes,'UTF8')::JSONB;
  IF v->>'schema_version' IS DISTINCT FROM '1' OR v->>'request_digest' IS DISTINCT FROM NEW.request_digest
     OR v->>'export_operation_id' IS DISTINCT FROM NEW.operation_id::TEXT
     OR COALESCE((v->>'bundle_bytes')::BIGINT,0)<=0
     OR octet_length(COALESCE(v->>'manifest_json','')) NOT BETWEEN 1 AND 1048576
     OR (v->>'bundle_bytes')::BIGINT>1073741824
     OR octet_length(COALESCE((v->'export_response')::TEXT,''))>1048576
     OR octet_length(COALESCE((v->'retention_inspection')::TEXT,''))>16384
     OR COALESCE((v->>'observed_at_ms')::BIGINT,0)<=0
     OR COALESCE((v->>'expires_at_ms')::BIGINT,0)<=COALESCE((v->>'observed_at_ms')::BIGINT,0)
     OR (v->>'expires_at_ms')::BIGINT-(v->>'observed_at_ms')::BIGINT>30000
     OR (v->>'bundle_bytes')::BIGINT+octet_length(v->>'manifest_json')<>NEW.charged_bytes
     OR NEW.charged_bytes>NEW.reserved_bytes
     OR COALESCE(v->>'bundle_digest','') !~ '^[0-9a-f]{64}$'
     OR COALESCE(v->>'producer_installation_id','') !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
     OR v->>'producer_installation_id'='00000000-0000-0000-0000-000000000000'
     OR v->>'manifest_digest' IS DISTINCT FROM encode(sha256(convert_to(v->>'manifest_json','UTF8')),'hex')
  THEN RAISE EXCEPTION 'source export verified fact mismatch'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION profiled_launch_registration_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE h profiled_native_registration_history%ROWTYPE; l profiled_launches%ROWTYPE; a execution_attempts%ROWTYPE; n BIGINT; b BIGINT; per_n BIGINT; per_b BIGINT;
BEGIN
 SELECT * INTO STRICT h FROM profiled_native_registration_history WHERE registration_id=NEW.registration_id;
 SELECT * INTO STRICT l FROM profiled_launches WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO STRICT a FROM execution_attempts WHERE attempt_id=NEW.attempt_id;
 IF h.node_id<>NEW.node_id OR h.boot_id<>NEW.boot_id OR a.node_id<>NEW.node_id OR a.kind<>'profiled_v1' OR NOT a.holds_task OR l.launch_binding#>>'{session,node_id}' IS DISTINCT FROM NEW.node_id::TEXT OR l.launch_binding#>>'{session,boot_id}' IS DISTINCT FROM NEW.boot_id::TEXT THEN RAISE EXCEPTION 'attempt registration binding'; END IF;
 IF (SELECT state#>>'{native,semantic_digest}' FROM execution_nodes WHERE node_id=NEW.node_id) IS DISTINCT FROM h.semantic_digest OR (SELECT state#>'{native,qualification}' FROM execution_nodes WHERE node_id=NEW.node_id) IS DISTINCT FROM (convert_from(h.metadata_bytes,'UTF8')::JSONB)->'qualification' THEN RAISE EXCEPTION 'attempt registration is not actual held metadata'; END IF;
 IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'link mutation isolation'; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-native-attempt-registration-link-v1',0));
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO n,b,per_n,per_b FROM sealed_history_usage('launch_registrations',NEW.node_id,0) u;
 IF n>=4096 OR b+64>262144 OR per_n>=256 OR per_b+64>16384 THEN RAISE EXCEPTION 'attempt registration link quota'; END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION profiled_phase_terminal_capacity_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n BIGINT; b BIGINT; pn BIGINT; pb BIGINT;
BEGIN
 IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'retained phase terminal capacity'; END IF;
 IF current_setting('transaction_isolation') NOT IN ('read committed','serializable') THEN RAISE EXCEPTION 'phase capacity requires current predicate visibility'; END IF;
 IF NOT EXISTS(SELECT 1 FROM profiled_phase_execution_grants g WHERE g.attempt_id=NEW.attempt_id AND g.node_id=NEW.node_id) THEN RAISE EXCEPTION 'actual phase grant capacity owner required'; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-phase-terminal-capacity-v1',0));
 SELECT u.total_rows,u.total_bytes,u.scope_rows,u.scope_bytes INTO n,b,pn,pb FROM sealed_history_usage('phase_terminal',NEW.node_id,0) u;
 IF n>=256 OR b+NEW.reserved_bytes>268435456 OR pn>=16 OR pb+NEW.reserved_bytes>16777216 THEN RAISE EXCEPTION 'phase terminal capacity exhausted before grant issuance'; END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION profiled_presence_operation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE g profiled_presence_storage%ROWTYPE; n profiled_presence_node_storage%ROWTYPE; incoming BIGINT; q JSONB; a JSONB; u RECORD; r RECORD;
BEGIN
 q:=convert_from(NEW.request_bytes,'UTF8')::JSONB; a:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 IF (q->>'operation_id')::UUID IS DISTINCT FROM NEW.operation_id OR (q#>>'{identity,node_id}')::UUID IS DISTINCT FROM NEW.node_id
 OR (q#>>'{identity,boot_id}')::UUID IS DISTINCT FROM NEW.boot_id OR (q->>'sequence')::BIGINT IS DISTINCT FROM NEW.sequence
 OR (CASE WHEN NEW.kind='inventory_v2' THEN a#>>'{inventory,request_digest}' ELSE a->>'request_digest' END) IS DISTINCT FROM NEW.request_digest
 OR (a#>>'{identity,node_id}')::UUID IS DISTINCT FROM NEW.node_id OR (a#>>'{identity,boot_id}')::UUID IS DISTINCT FROM NEW.boot_id
 OR (a->>'recorded_at_ms')::BIGINT IS DISTINCT FROM NEW.recorded_at_ms
 OR (a->>'expires_at_ms')::BIGINT IS DISTINCT FROM NEW.expires_at_ms OR q->>'schema_version' IS DISTINCT FROM '1' OR a->>'schema_version' IS DISTINCT FROM '1'
 THEN RAISE EXCEPTION 'presence operation body scope'; END IF;
 IF NEW.kind='inventory_v2' AND ((a#>>'{inventory,revision}')::BIGINT IS DISTINCT FROM NEW.inventory_revision OR a#>>'{inventory,snapshot_digest}' IS DISTINCT FROM NEW.snapshot_digest)
 THEN RAISE EXCEPTION 'inventory body receipt scope'; END IF;
 IF NEW.kind='presence' AND ((q#>>'{binding,attempt_id}')::UUID IS DISTINCT FROM NEW.attempt_id OR (q->>'lease_id')::UUID IS DISTINCT FROM NEW.lease_id OR (q->>'grant_id')::UUID IS DISTINCT FROM NEW.grant_id
 OR a->'binding' IS DISTINCT FROM q->'binding' OR (a->>'operation_id')::UUID IS DISTINCT FROM NEW.operation_id OR (a->>'sequence')::BIGINT IS DISTINCT FROM NEW.sequence)
 THEN RAISE EXCEPTION 'presence attempt lease scope'; END IF;
 incoming:=octet_length(NEW.request_bytes)::BIGINT+octet_length(NEW.receipt_bytes)::BIGINT+1024;
 SELECT * INTO g FROM profiled_presence_storage WHERE singleton FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'presence storage owner missing'; END IF;
 INSERT INTO profiled_presence_node_storage(node_id) VALUES(NEW.node_id) ON CONFLICT DO NOTHING;
 SELECT * INTO n FROM profiled_presence_node_storage WHERE node_id=NEW.node_id FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'presence node storage missing'; END IF;
 SELECT * INTO u FROM sealed_history_usage('presence_operations',NEW.node_id,NEW.recorded_at_ms);
 SELECT * INTO r FROM sealed_history_usage('presence_rate',NEW.node_id,NEW.recorded_at_ms);
 IF u.total_rows>=65536 OR u.scope_rows>=4096 OR u.total_bytes+incoming>268435456 OR u.scope_bytes+incoming>16777216
 OR r.total_rows>=11520 OR r.scope_rows>=720
 THEN RAISE EXCEPTION 'scoped presence retention capacity' USING ERRCODE='54000'; END IF;
 UPDATE profiled_presence_storage SET operations=operations+1,logical_bytes=logical_bytes+incoming WHERE singleton;
 UPDATE profiled_presence_node_storage SET operations=operations+1,logical_bytes=logical_bytes+incoming WHERE node_id=NEW.node_id;
 RETURN NEW;
END $$;
UPDATE meta SET value='104' WHERE key='state_format_version';
