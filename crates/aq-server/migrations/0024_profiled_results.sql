-- Report claims only. No task/source/effect release or graph transition.
CREATE TABLE profiled_result_reports (
 report_id UUID PRIMARY KEY CHECK(report_id<>'00000000-0000-0000-0000-000000000000'),
 attempt_id UUID NOT NULL REFERENCES profiled_stopped_receipts(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 operation_id UUID NOT NULL CHECK(operation_id<>'00000000-0000-0000-0000-000000000000'),
 supersedes UUID REFERENCES profiled_result_reports(report_id),
 report_bytes BYTEA NOT NULL CHECK(octet_length(report_bytes) BETWEEN 1 AND 65536),
 report_digest TEXT NOT NULL CHECK(report_digest=encode(sha256(report_bytes),'hex')),
 receipt_bytes BYTEA NOT NULL CHECK(octet_length(receipt_bytes) BETWEEN 1 AND 65536),
 outcome_artifact_id UUID NOT NULL REFERENCES admission_artifacts(artifact_id),
 UNIQUE(node_id,operation_id), UNIQUE(attempt_id,report_id)
);
CREATE INDEX profiled_results_attempt ON profiled_result_reports(attempt_id);
CREATE TABLE profiled_result_heads (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_stopped_receipts(attempt_id),
 report_id UUID NOT NULL UNIQUE,
 FOREIGN KEY(attempt_id,report_id) REFERENCES profiled_result_reports(attempt_id,report_id)
);
CREATE TRIGGER profiled_result_immutable BEFORE UPDATE OR DELETE ON profiled_result_reports
 FOR EACH ROW EXECUTE FUNCTION profiled_sealed_row_guard();
CREATE FUNCTION profiled_result_insert_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE q JSONB; r JSONB; b JSONB; m JSONB; s JSONB; stopped JSONB;
 h JSONB; held BOOLEAN; head UUID; n BIGINT; used BIGINT; a admission_artifacts%ROWTYPE;
BEGIN
 -- Also serialize direct owner inserts; no authority fields are changed.
 SELECT holds_task INTO STRICT held FROM execution_attempts WHERE attempt_id=NEW.attempt_id FOR UPDATE;
 SELECT state INTO STRICT h FROM profiled_authority_hot WHERE attempt_id=NEW.attempt_id;
 SELECT launch_binding,launch_manifest INTO STRICT b,m FROM profiled_launches WHERE attempt_id=NEW.attempt_id;
 SELECT convert_from(request_bytes,'UTF8')::JSONB,
 jsonb_build_object('id','aq.native-stopped/'||NEW.attempt_id::TEXT,'version','1','digest',encode(sha256(receipt_bytes),'hex'))
 INTO STRICT s,stopped FROM profiled_stopped_receipts WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO STRICT a FROM admission_artifacts WHERE artifact_id=NEW.outcome_artifact_id;
 q:=convert_from(NEW.report_bytes,'UTF8')::JSONB;r:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 SELECT report_id INTO head FROM profiled_result_heads WHERE attempt_id=NEW.attempt_id;
 SELECT count(*),COALESCE(sum(octet_length(report_bytes)),0) INTO n,used FROM profiled_result_reports WHERE attempt_id=NEW.attempt_id;
 IF NOT held OR h->>'status' IS DISTINCT FROM 'finished'
 OR h->'stop' IS DISTINCT FROM jsonb_build_object('kind','stopped','receipt',stopped)
 OR q->>'schema_version' IS DISTINCT FROM '1' OR r->>'schema_version' IS DISTINCT FROM '1'
 OR q->'binding' IS DISTINCT FROM b OR r->'binding' IS DISTINCT FROM b
 OR q#>>'{identity,node_id}' IS DISTINCT FROM NEW.node_id::TEXT
 OR q#>>'{identity,boot_id}' IS DISTINCT FROM b#>>'{session,boot_id}'
 OR b#>>'{session,node_id}' IS DISTINCT FROM NEW.node_id::TEXT
 OR q->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT OR r->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT
 OR q->>'report_id' IS DISTINCT FROM NEW.report_id::TEXT OR r->>'report_id' IS DISTINCT FROM NEW.report_id::TEXT
 OR (q->>'supersedes')::UUID IS DISTINCT FROM NEW.supersedes OR NEW.supersedes IS DISTINCT FROM head
 OR q->'stop' IS DISTINCT FROM s->'evidence'
 OR r->>'report_digest' IS DISTINCT FROM NEW.report_digest
 OR r->>'disposition' IS DISTINCT FROM 'recorded'
 OR r->'stopped_receipt' IS DISTINCT FROM stopped
 OR r->'expected_outcome' IS DISTINCT FROM m#>'{bindings,outcome}'
 OR COALESCE((r->>'recorded_at_ms')::BIGINT,0)<=0
 OR a.owner_kind<>'outcome' OR a.owner_key IS DISTINCT FROM m#>>'{bindings,project_id}'
 OR a.descriptor->'artifact' IS DISTINCT FROM m#>'{bindings,outcome}' OR a.body IS NULL
 OR NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts WHERE attempt_id=NEW.attempt_id AND artifact_id=NEW.outcome_artifact_id)
 OR n>=64 OR used+octet_length(NEW.report_bytes)>4194304
 THEN RAISE EXCEPTION 'native report requires exact stopped/outcome owner, correction head and quota'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_result_insert_guard BEFORE INSERT ON profiled_result_reports
 FOR EACH ROW EXECUTE FUNCTION profiled_result_insert_guard();
CREATE FUNCTION profiled_result_head_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE previous UUID; owner UUID;
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'retained result head'; END IF;
 SELECT attempt_id,supersedes INTO STRICT owner,previous FROM profiled_result_reports WHERE report_id=NEW.report_id;
 IF NEW.attempt_id<>owner THEN RAISE EXCEPTION 'result head scope'; END IF;
 IF TG_OP='INSERT' THEN
  IF previous IS NOT NULL THEN RAISE EXCEPTION 'first report head cannot skip predecessor'; END IF;
 ELSE
  IF NEW.attempt_id<>OLD.attempt_id OR previous IS DISTINCT FROM OLD.report_id OR NEW.report_id=OLD.report_id
  THEN RAISE EXCEPTION 'result correction must follow current head'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_result_head_guard BEFORE INSERT OR UPDATE OR DELETE ON profiled_result_heads
 FOR EACH ROW EXECUTE FUNCTION profiled_result_head_guard();
CREATE FUNCTION profiled_result_atomic_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM profiled_result_heads WHERE attempt_id=NEW.attempt_id AND report_id=NEW.report_id)
 THEN RAISE EXCEPTION 'report and correction head must commit together'; END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER profiled_result_atomic_guard AFTER INSERT ON profiled_result_reports
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION profiled_result_atomic_guard();
UPDATE meta SET value='24' WHERE key='state_format_version';
