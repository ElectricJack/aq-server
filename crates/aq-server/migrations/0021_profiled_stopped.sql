-- Actual post-Started stopped evidence. This closes execution authority, not the
-- task claim, source retention, effects or graph task lifecycle.
ALTER TABLE profiled_owner_journals DROP CONSTRAINT profiled_owner_journals_phase_check;
ALTER TABLE profiled_owner_journals ADD CONSTRAINT profiled_owner_journals_phase_check
 CHECK(phase IN ('reserved','prepared','started','stopped'));
CREATE TABLE profiled_stopped_receipts (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_started_receipts(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 operation_id UUID NOT NULL,
 request_bytes BYTEA NOT NULL CHECK(octet_length(request_bytes) BETWEEN 1 AND 16384),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$' AND request_digest=encode(sha256(request_bytes),'hex')),
 proof_bytes BYTEA NOT NULL CHECK(octet_length(proof_bytes) BETWEEN 1 AND 16384),
 proof_digest TEXT NOT NULL CHECK(proof_digest ~ '^[0-9a-f]{64}$' AND proof_digest=encode(sha256(proof_bytes),'hex')),
 receipt_bytes BYTEA NOT NULL CHECK(octet_length(receipt_bytes) BETWEEN 1 AND 16384),
 before_record_revision BIGINT NOT NULL CHECK(before_record_revision>0),
 before_grant_revision BIGINT NOT NULL CHECK(before_grant_revision>0),
 UNIQUE(node_id,operation_id)
);
CREATE TRIGGER profiled_stopped_immutable BEFORE UPDATE OR DELETE ON profiled_stopped_receipts
 FOR EACH ROW EXECUTE FUNCTION profiled_sealed_row_guard();

CREATE OR REPLACE FUNCTION profiled_owner_journal_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE h profiled_authority_hot%ROWTYPE; b JSONB; held BOOLEAN;
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'retained owner journal head'; END IF;
 SELECT hot.state,l.launch_binding,a.holds_task INTO h.state,b,held FROM profiled_authority_hot hot
 JOIN profiled_launches l USING(attempt_id) JOIN execution_attempts a USING(attempt_id)
 WHERE hot.attempt_id=NEW.attempt_id;
 IF h.state IS NULL OR NEW.binding IS DISTINCT FROM b OR NOT held
 OR NEW.guardian_id::TEXT IS DISTINCT FROM h.state#>>'{owner,guardian_id}'
 OR NEW.launch_id::TEXT IS DISTINCT FROM h.state#>>'{owner,launch_id}'
 OR NEW.reservation_id::TEXT IS DISTINCT FROM h.state#>>'{owner,reservation_id}'
 THEN RAISE EXCEPTION 'owner journal authority mismatch'; END IF;
 IF NEW.phase='stopped' THEN
   IF h.state->>'status' NOT IN ('granted','cancelling','unknown')
   OR h.state#>>'{stop,kind}' NOT IN ('running','unknown')
   OR NEW.grant_id::TEXT IS DISTINCT FROM h.state#>>'{grant,grant_id}'
   OR NOT EXISTS(SELECT 1 FROM profiled_stopped_receipts r WHERE r.attempt_id=NEW.attempt_id
     AND (convert_from(r.request_bytes,'UTF8')::JSONB)#>>'{evidence,journal,revision}'=NEW.revision::TEXT
     AND (convert_from(r.request_bytes,'UTF8')::JSONB)#>>'{evidence,journal,digest}'=NEW.digest)
   THEN RAISE EXCEPTION 'stopped journal needs actual stopped owner receipt'; END IF;
 ELSE
   IF h.state#>>'{stop,kind}' IS DISTINCT FROM 'unknown'
   OR h.state->'revoked' IS DISTINCT FROM 'false'::JSONB OR h.state->'cancellation_id' IS DISTINCT FROM 'null'::JSONB
   OR (NEW.phase='reserved' AND (h.state->>'status'<>'preparing' OR h.state->'grant'<>'null'::JSONB))
   OR (NEW.phase<>'reserved' AND (h.state->>'status'<>'granted' OR NEW.grant_id::TEXT IS DISTINCT FROM h.state#>>'{grant,grant_id}'))
   THEN RAISE EXCEPTION 'owner journal grant/phase mismatch'; END IF;
 END IF;
 IF TG_OP='INSERT' THEN
   IF NEW.phase<>'reserved' THEN RAISE EXCEPTION 'journal requires reserved origin'; END IF;
 ELSE
   IF ROW(NEW.attempt_id,NEW.binding,NEW.guardian_id,NEW.launch_id,NEW.reservation_id)
      IS DISTINCT FROM ROW(OLD.attempt_id,OLD.binding,OLD.guardian_id,OLD.launch_id,OLD.reservation_id)
      OR NEW.revision<=OLD.revision
      OR NOT ((OLD.phase='reserved' AND NEW.phase='prepared') OR(OLD.phase='prepared' AND NEW.phase='started') OR(OLD.phase='started' AND NEW.phase='stopped'))
      OR (OLD.grant_id IS NOT NULL AND NEW.grant_id IS DISTINCT FROM OLD.grant_id)
   THEN RAISE EXCEPTION 'owner journal cannot rewrite scope, regress, or skip a phase'; END IF;
 END IF;
 IF NEW.phase='started' AND NOT EXISTS(SELECT 1 FROM profiled_started_receipts r
   WHERE r.attempt_id=NEW.attempt_id
   AND (convert_from(r.request_bytes,'UTF8')::JSONB)->>'journal_revision'=NEW.revision::TEXT
   AND (convert_from(r.request_bytes,'UTF8')::JSONB)->>'journal_digest'=NEW.digest)
 THEN RAISE EXCEPTION 'started journal requires its original started receipt'; END IF;
 RETURN NEW;
END $$;

CREATE FUNCTION profiled_stopped_insert_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE j profiled_owner_journals%ROWTYPE; h profiled_authority_hot%ROWTYPE;
 q JSONB; r JSONB; z JSONB; o JSONB; held BOOLEAN;
BEGIN
 SELECT * INTO STRICT j FROM profiled_owner_journals WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO STRICT h FROM profiled_authority_hot WHERE attempt_id=NEW.attempt_id;
 SELECT holds_task INTO STRICT held FROM execution_attempts WHERE attempt_id=NEW.attempt_id;
 SELECT convert_from(request_bytes,'UTF8')::JSONB INTO STRICT z FROM profiled_started_receipts WHERE attempt_id=NEW.attempt_id;
 q:=convert_from(NEW.request_bytes,'UTF8')::JSONB; r:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 o:=q#>'{proof,observation}';
 IF NOT held OR j.phase<>'started' OR NEW.before_record_revision<>h.record_revision OR NEW.before_grant_revision<>h.grant_revision
 OR h.state->>'status' NOT IN ('granted','cancelling','unknown') OR h.state#>>'{stop,kind}' NOT IN ('running','unknown')
 OR q->>'schema_version' IS DISTINCT FROM '1' OR r->>'schema_version' IS DISTINCT FROM '1'
 OR q#>'{evidence,binding}' IS DISTINCT FROM j.binding OR r->'binding' IS DISTINCT FROM j.binding
 OR q#>>'{evidence,binding,session,node_id}' IS DISTINCT FROM NEW.node_id::TEXT
 OR q#>>'{evidence,stop_operation_id}' IS DISTINCT FROM NEW.operation_id::TEXT OR r->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT
 OR q#>>'{evidence,guardian_id}' IS DISTINCT FROM j.guardian_id::TEXT OR q#>>'{evidence,launch_id}' IS DISTINCT FROM j.launch_id::TEXT
 OR q->>'reservation_id' IS DISTINCT FROM j.reservation_id::TEXT
 OR q#>>'{evidence,disposition,kind}' IS DISTINCT FROM 'stopped'
 OR q#>>'{evidence,disposition,grant_id}' IS DISTINCT FROM j.grant_id::TEXT
 OR q#>>'{evidence,disposition,grant_id}' IS DISTINCT FROM h.state#>>'{grant,grant_id}'
 OR r->>'grant_id' IS DISTINCT FROM j.grant_id::TEXT
 OR q#>>'{evidence,journal,kind}' IS DISTINCT FROM 'recorded'
 OR COALESCE((q#>>'{evidence,journal,revision}')::BIGINT,0)<=j.revision
 OR q#>>'{evidence,disposition,stop_receipt_digest}' IS DISTINCT FROM NEW.proof_digest
 OR convert_from(NEW.proof_bytes,'UTF8')::JSONB IS DISTINCT FROM q->'proof'
 OR q->'runtime' IS DISTINCT FROM z->'runtime' OR q->'qualification' IS DISTINCT FROM z->'qualification'
 OR o->'guardian' IS DISTINCT FROM z#>'{observation,guardian}' OR o->'pane' IS DISTINCT FROM z#>'{observation,pane}'
 OR o->>'phase' IS DISTINCT FROM 'stopped' OR o->'kernel_wait_echild' IS DISTINCT FROM 'true'::JSONB
 OR jsonb_typeof(o->'root_status') IS DISTINCT FROM 'number'
 OR COALESCE((o->>'reaped_count')::BIGINT,0)<1
 OR COALESCE((o->>'reaped_count')::BIGINT,0)<(z#>>'{observation,reaped_count}')::BIGINT
 OR COALESCE((o->>'observed_boot_ms')::BIGINT,0)<(z#>>'{observation,observed_boot_ms}')::BIGINT
 OR COALESCE((o->>'permit_sequence')::BIGINT,0)<(z#>>'{observation,permit_sequence}')::BIGINT
 OR COALESCE((o->>'permit_deadline_boot_ms')::BIGINT,0)<1
 OR (o->'permit_sequence'=z#>'{observation,permit_sequence}' AND o->'permit_deadline_boot_ms' IS DISTINCT FROM z#>'{observation,permit_deadline_boot_ms}')
 OR r->>'request_digest' IS DISTINCT FROM NEW.request_digest
 OR COALESCE((r->>'recorded_at_ms')::BIGINT,0)<=0
 OR (r->>'authority_revision_at_record')::BIGINT IS DISTINCT FROM h.authority_revision+1
 THEN RAISE EXCEPTION 'stopped receipt requires exact original started owner'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_stopped_insert_guard BEFORE INSERT ON profiled_stopped_receipts
 FOR EACH ROW EXECUTE FUNCTION profiled_stopped_insert_guard();
CREATE FUNCTION profiled_stopped_atomic_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE h profiled_authority_hot%ROWTYPE; j profiled_owner_journals%ROWTYPE; q JSONB; r JSONB; ref JSONB;
BEGIN
 SELECT * INTO STRICT h FROM profiled_authority_hot WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO STRICT j FROM profiled_owner_journals WHERE attempt_id=NEW.attempt_id;
 q:=convert_from(NEW.request_bytes,'UTF8')::JSONB; r:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
 ref:=jsonb_build_object('id','aq.native-stopped/'||NEW.attempt_id::TEXT,'version','1','digest',encode(sha256(NEW.receipt_bytes),'hex'));
 IF h.record_revision<>NEW.before_record_revision+1 OR h.grant_revision<>NEW.before_grant_revision
 OR h.state->>'status'<>'finished' OR h.state->'stop' IS DISTINCT FROM jsonb_build_object('kind','stopped','receipt',ref)
 OR h.authority_revision<>(r->>'authority_revision_at_record')::BIGINT
 OR j.phase<>'stopped' OR j.revision<>(q#>>'{evidence,journal,revision}')::BIGINT OR j.digest IS DISTINCT FROM q#>>'{evidence,journal,digest}'
 THEN RAISE EXCEPTION 'stopped receipt, journal and hot transition must commit together'; END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER profiled_stopped_atomic_guard AFTER INSERT ON profiled_stopped_receipts
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION profiled_stopped_atomic_guard();
CREATE FUNCTION profiled_stopped_projection_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.state#>>'{stop,kind}'='stopped' AND OLD.state#>>'{stop,kind}'<>'stopped'
 AND EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=NEW.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_stopped_receipts r JOIN profiled_owner_journals j USING(attempt_id)
  WHERE r.attempt_id=NEW.attempt_id AND r.before_record_revision=OLD.record_revision AND j.phase='stopped'
  AND NEW.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object('id','aq.native-stopped/'||NEW.attempt_id::TEXT,'version','1','digest',encode(sha256(r.receipt_bytes),'hex'))))
 THEN RAISE EXCEPTION 'native stopped projection requires original stopped receipt'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_stopped_projection_guard BEFORE UPDATE ON profiled_authority_hot
 FOR EACH ROW EXECUTE FUNCTION profiled_stopped_projection_guard();
UPDATE meta SET value='21' WHERE key='state_format_version';
