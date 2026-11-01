-- Supplemental legacy-report media only. No task/result truth is copied here.
-- File durability and live physical-job fences belong to the managed-file owner.
CREATE TABLE result_media_storage (
 singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
 storage_id UUID NOT NULL UNIQUE CHECK(storage_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 root_fingerprint TEXT NOT NULL CHECK(root_fingerprint ~ '^[0-9a-f]{64}$'),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms BETWEEN 1 AND 9007199254740991)
);
CREATE FUNCTION guard_result_media_storage() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'result media storage identity is immutable';
END $$;
CREATE TRIGGER result_media_storage_update_guard BEFORE UPDATE ON result_media_storage FOR EACH ROW EXECUTE FUNCTION guard_result_media_storage();
CREATE TRIGGER result_media_storage_delete_guard BEFORE DELETE ON result_media_storage FOR EACH ROW EXECUTE FUNCTION guard_result_media_storage();
CREATE TRIGGER result_media_storage_truncate_guard BEFORE TRUNCATE ON result_media_storage FOR EACH STATEMENT EXECUTE FUNCTION guard_result_media_storage();

CREATE TABLE result_media_budgets (
 scope_key TEXT PRIMARY KEY CHECK(scope_key='installation' OR scope_key ~ '^project/[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$'),
 limit_bytes BIGINT NOT NULL CHECK(limit_bytes BETWEEN 1 AND 268435456),
 reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK(reserved_bytes>=0),
 retained_bytes BIGINT NOT NULL DEFAULT 0 CHECK(retained_bytes>=0),
 historical_intents BIGINT NOT NULL DEFAULT 0 CHECK(historical_intents>=0),
 CONSTRAINT result_media_budget_capacity_check CHECK(reserved_bytes+retained_bytes<=limit_bytes),
 CHECK(scope_key='installation' OR limit_bytes<=67108864),
 CHECK(historical_intents<=CASE WHEN scope_key='installation' THEN 4096 ELSE 512 END)
);
INSERT INTO result_media_budgets(scope_key,limit_bytes) VALUES('installation',268435456);
CREATE FUNCTION guard_result_media_budget() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'result media budget history cannot be deleted'; END IF;
 IF TG_OP='INSERT' THEN
  IF NEW.reserved_bytes<>0 OR NEW.retained_bytes<>0 OR NEW.historical_intents<>0 THEN RAISE EXCEPTION 'new media budget must be empty'; END IF;
 ELSE
  IF NEW.scope_key IS DISTINCT FROM OLD.scope_key OR NEW.historical_intents<OLD.historical_intents THEN RAISE EXCEPTION 'media budget identity/history is immutable'; END IF;
  IF ROW(NEW.reserved_bytes,NEW.retained_bytes,NEW.historical_intents) IS DISTINCT FROM ROW(OLD.reserved_bytes,OLD.retained_bytes,OLD.historical_intents) AND pg_trigger_depth()<2 THEN
   RAISE EXCEPTION 'media charges change only through intent owner transitions';
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER result_media_budget_insert_guard BEFORE INSERT ON result_media_budgets FOR EACH ROW EXECUTE FUNCTION guard_result_media_budget();
CREATE TRIGGER result_media_budget_update_guard BEFORE UPDATE ON result_media_budgets FOR EACH ROW EXECUTE FUNCTION guard_result_media_budget();
CREATE TRIGGER result_media_budget_delete_guard BEFORE DELETE ON result_media_budgets FOR EACH ROW EXECUTE FUNCTION guard_result_media_budget();
CREATE TRIGGER result_media_budget_truncate_guard BEFORE TRUNCATE ON result_media_budgets FOR EACH STATEMENT EXECUTE FUNCTION guard_result_media_budget();

CREATE TABLE result_media_intents (
 operation_id UUID PRIMARY KEY CHECK(operation_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 actor_id UUID NOT NULL REFERENCES users(id),
 actor_identity JSONB NOT NULL CHECK(jsonb_typeof(actor_identity)='object' AND octet_length(actor_identity::TEXT)<=8192),
 report_id UUID NOT NULL REFERENCES profiled_result_reports(report_id),
 report_revision TEXT NOT NULL CHECK(report_revision ~ '^[0-9a-f]{64}$'),
 project_id UUID NOT NULL CHECK(project_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 report_binding JSONB NOT NULL CHECK(jsonb_typeof(report_binding)='object' AND octet_length(report_binding::TEXT)<=16384),
 storage_id UUID NOT NULL REFERENCES result_media_storage(storage_id),
 rendering_policy TEXT NOT NULL CHECK(rendering_policy='aq.png.rgba8-srgb-v1'),
 admission_limits JSONB NOT NULL CHECK(jsonb_typeof(admission_limits)='object'),
 request_bytes BYTEA NOT NULL CHECK(octet_length(request_bytes) BETWEEN 1 AND 16384),
 request_digest TEXT NOT NULL CHECK(request_digest='sha256:'||encode(sha256(request_bytes),'hex')),
 input_digest TEXT NOT NULL CHECK(input_digest ~ '^sha256:[0-9a-f]{64}$'),
 input_bytes BIGINT NOT NULL CHECK(input_bytes BETWEEN 1 AND 1048576),
 filename TEXT NOT NULL CHECK(octet_length(filename) BETWEEN 1 AND 256),
 capture JSONB NOT NULL CHECK(jsonb_typeof(capture) IN ('object','null')),
 state TEXT NOT NULL CHECK(state IN ('reserved','received','finalizing','retained','rejected','expired')),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms BETWEEN 1 AND 9007199254740991),
 upload_expires_at_ms BIGINT NOT NULL,
 retain_until_ms BIGINT NOT NULL,
 transition_at_ms BIGINT NOT NULL CHECK(transition_at_ms BETWEEN 1 AND 9007199254740991),
 reserved_bytes BIGINT NOT NULL CHECK(reserved_bytes BETWEEN 0 AND 10485760),
 retained_bytes BIGINT NOT NULL DEFAULT 0 CHECK(retained_bytes BETWEEN 0 AND 10485760),
 lease_id UUID,
 lease_expires_at_ms BIGINT,
 processing_attempts INTEGER NOT NULL DEFAULT 0 CHECK(processing_attempts BETWEEN 0 AND 8),
 received_at_ms BIGINT,
 rejection TEXT CHECK(rejection IN ('invalid_png','unsupported_png','input_mismatch','pixel_limit','processing_limit','storage_failure')),
 receipt_bytes BYTEA CHECK(octet_length(receipt_bytes) BETWEEN 1 AND 16384),
 receipt_digest TEXT,
 file_generation UUID NOT NULL CHECK(file_generation=operation_id),
 withheld_at_ms BIGINT,
 cleanup_state TEXT NOT NULL DEFAULT 'none' CHECK(cleanup_state IN ('none','running','blocked','done')),
 cleanup_generation UUID,
 cleanup_completed_at_ms BIGINT,
 CHECK(upload_expires_at_ms>created_at_ms AND upload_expires_at_ms-created_at_ms<=1800000),
 CHECK(retain_until_ms>upload_expires_at_ms AND retain_until_ms-created_at_ms<=604800000),
 CHECK(transition_at_ms>=created_at_ms),
 CONSTRAINT result_media_lease_pair_check CHECK((lease_id IS NULL)=(lease_expires_at_ms IS NULL)),
 CHECK(lease_expires_at_ms IS NULL OR (lease_expires_at_ms>created_at_ms AND lease_expires_at_ms<=upload_expires_at_ms)),
 CONSTRAINT result_media_receipt_pair_check CHECK((receipt_bytes IS NULL)=(receipt_digest IS NULL)),
 CHECK(receipt_digest IS NULL OR receipt_digest='sha256:'||encode(sha256(receipt_bytes),'hex')),
 CONSTRAINT result_media_retained_state_check CHECK((state='retained')=(receipt_bytes IS NOT NULL)),
 CHECK((state='rejected')=(rejection IS NOT NULL)),
 CHECK(state<>'retained' OR reserved_bytes=0),
 CHECK(state NOT IN ('retained','rejected','expired') OR lease_id IS NULL),
 CHECK(state NOT IN ('received','finalizing','retained') OR (received_at_ms IS NOT NULL AND received_at_ms>=created_at_ms AND received_at_ms<upload_expires_at_ms)),
 CHECK(state='retained' OR retained_bytes=0),
 CHECK(cleanup_state<>'done' OR (reserved_bytes=0 AND retained_bytes=0 AND cleanup_generation IS NOT NULL AND cleanup_completed_at_ms IS NOT NULL)),
 CHECK(cleanup_state NOT IN ('running','blocked') OR cleanup_generation IS NOT NULL)
);
CREATE INDEX result_media_exact_report ON result_media_intents(report_id,report_revision,operation_id) WHERE state='retained';
CREATE INDEX result_media_actor_lookup ON result_media_intents(actor_id,operation_id);
CREATE INDEX result_media_project_budget ON result_media_intents(project_id);
CREATE INDEX result_media_pending_cleanup ON result_media_intents(upload_expires_at_ms,operation_id) WHERE state IN ('reserved','received','finalizing','rejected','expired') AND cleanup_state<>'done';
CREATE INDEX result_media_retained_cleanup ON result_media_intents(retain_until_ms,operation_id) WHERE state='retained' AND cleanup_state<>'done';

CREATE FUNCTION guard_result_media_intent() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE request JSONB; receipt JSONB; variant JSONB; render JSONB; preview JSONB; spec JSONB; recorded_digest TEXT; current_actor JSONB; role_name TEXT;
 charge_reserved BIGINT; charge_retained BIGINT; history_delta BIGINT; count_subject BIGINT; project_key TEXT; logical_bytes BIGINT;
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'result media intent/receipt history cannot be deleted'; END IF;
 IF TG_OP='UPDATE' THEN
  IF ROW(NEW.operation_id,NEW.actor_id,NEW.actor_identity,NEW.report_id,NEW.report_revision,NEW.project_id,NEW.report_binding,NEW.storage_id,NEW.rendering_policy,NEW.admission_limits,NEW.request_bytes,NEW.request_digest,NEW.input_digest,NEW.input_bytes,NEW.filename,NEW.capture,NEW.created_at_ms,NEW.upload_expires_at_ms,NEW.retain_until_ms,NEW.file_generation)
   IS DISTINCT FROM ROW(OLD.operation_id,OLD.actor_id,OLD.actor_identity,OLD.report_id,OLD.report_revision,OLD.project_id,OLD.report_binding,OLD.storage_id,OLD.rendering_policy,OLD.admission_limits,OLD.request_bytes,OLD.request_digest,OLD.input_digest,OLD.input_bytes,OLD.filename,OLD.capture,OLD.created_at_ms,OLD.upload_expires_at_ms,OLD.retain_until_ms,OLD.file_generation) THEN
   RAISE EXCEPTION 'result media intent identity is immutable';
  END IF;
  IF OLD.receipt_bytes IS NOT NULL AND ROW(NEW.receipt_bytes,NEW.receipt_digest) IS DISTINCT FROM ROW(OLD.receipt_bytes,OLD.receipt_digest) THEN RAISE EXCEPTION 'media retention receipt is immutable'; END IF;
  IF NEW.transition_at_ms<OLD.transition_at_ms OR NEW.processing_attempts<OLD.processing_attempts OR NEW.processing_attempts>OLD.processing_attempts+1 THEN RAISE EXCEPTION 'media history cannot regress'; END IF;
  IF OLD.received_at_ms IS NOT NULL AND NEW.received_at_ms IS DISTINCT FROM OLD.received_at_ms THEN RAISE EXCEPTION 'received media input is immutable'; END IF;
  IF OLD.state IN ('retained','rejected','expired') AND NEW.state<>OLD.state THEN RAISE EXCEPTION 'terminal media state cannot be replayed'; END IF;
  IF OLD.state='received' AND NEW.state='reserved' OR OLD.state='finalizing' AND NEW.state IN ('reserved','received') THEN RAISE EXCEPTION 'media state cannot regress'; END IF;
  IF OLD.cleanup_state='done' AND NEW IS DISTINCT FROM OLD THEN RAISE EXCEPTION 'completed media cleanup is immutable'; END IF;
  IF OLD.cleanup_state='none' AND NEW.cleanup_state NOT IN ('none','running') OR OLD.cleanup_state='running' AND NEW.cleanup_state NOT IN ('running','blocked','done') OR OLD.cleanup_state='blocked' AND NEW.cleanup_state NOT IN ('blocked','running','done') THEN RAISE EXCEPTION 'media cleanup cannot regress'; END IF;
  IF NEW.lease_id IS NOT NULL AND (OLD.lease_id IS NULL OR NEW.lease_id<>OLD.lease_id) AND (NEW.lease_expires_at_ms<=NEW.transition_at_ms OR NEW.cleanup_state<>'none') THEN RAISE EXCEPTION 'media new lease is not live'; END IF;
  IF NEW.lease_id IS NOT NULL AND NEW.lease_id=OLD.lease_id AND NEW.lease_expires_at_ms IS DISTINCT FROM OLD.lease_expires_at_ms THEN RAISE EXCEPTION 'media lease deadline is immutable'; END IF;
  IF NEW.lease_id IS NOT NULL AND OLD.lease_id IS NOT NULL AND NEW.lease_id<>OLD.lease_id AND NEW.transition_at_ms<OLD.lease_expires_at_ms THEN RAISE EXCEPTION 'live media lease cannot be replaced'; END IF;
  IF NEW.processing_attempts>OLD.processing_attempts AND (NEW.state<>'finalizing' OR NEW.lease_id IS NULL OR NEW.lease_id IS NOT DISTINCT FROM OLD.lease_id) THEN RAISE EXCEPTION 'media processing needs a new fenced lease'; END IF;
  IF NEW.cleanup_state IN ('running','done') AND NEW.cleanup_state<>OLD.cleanup_state THEN
   IF NEW.transition_at_ms<CASE WHEN OLD.state='retained' THEN OLD.retain_until_ms WHEN OLD.state='rejected' THEN OLD.transition_at_ms ELSE OLD.upload_expires_at_ms END OR NEW.lease_id IS NOT NULL THEN RAISE EXCEPTION 'media cleanup is premature'; END IF;
  END IF;
  IF NEW.reserved_bytes<OLD.reserved_bytes AND NOT (NEW.state='retained' AND OLD.state='finalizing' OR NEW.cleanup_state='done') THEN RAISE EXCEPTION 'media reservation cannot be discarded'; END IF;
  IF NEW.reserved_bytes>OLD.reserved_bytes THEN RAISE EXCEPTION 'media reservation cannot grow'; END IF;
  IF NEW.retained_bytes<OLD.retained_bytes AND NEW.cleanup_state<>'done' THEN RAISE EXCEPTION 'media retained charge needs confirmed cleanup'; END IF;
  IF NEW.retained_bytes>OLD.retained_bytes AND NOT (OLD.state='finalizing' AND NEW.state='retained' AND OLD.retained_bytes=0) THEN RAISE EXCEPTION 'media retained charge cannot grow'; END IF;
  charge_reserved:=NEW.reserved_bytes-OLD.reserved_bytes;charge_retained:=NEW.retained_bytes-OLD.retained_bytes;history_delta:=0;
 ELSE
  IF NEW.state<>'reserved' OR NEW.reserved_bytes<>10485760 OR NEW.retained_bytes<>0 OR NEW.processing_attempts<>0 OR NEW.lease_id IS NOT NULL OR NEW.cleanup_state<>'none' OR NEW.receipt_bytes IS NOT NULL OR NEW.transition_at_ms<>NEW.created_at_ms THEN RAISE EXCEPTION 'new media intent must reserve before IO'; END IF;
  request:=convert_from(NEW.request_bytes,'UTF8')::JSONB;
  IF jsonb_typeof(request)<>'object' OR NOT request ?& ARRAY['operation_id','input_digest','input_bytes','filename','capture'] OR request-ARRAY['operation_id','input_digest','input_bytes','filename','capture']<>'{}'::JSONB
   OR request->>'operation_id' IS DISTINCT FROM NEW.operation_id::TEXT OR request->>'input_digest' IS DISTINCT FROM NEW.input_digest OR (request->>'input_bytes')::BIGINT IS DISTINCT FROM NEW.input_bytes OR request->>'filename' IS DISTINCT FROM NEW.filename OR request->'capture' IS DISTINCT FROM NEW.capture THEN RAISE EXCEPTION 'media request identity mismatch'; END IF;
  SELECT r.report_digest,l.execution_spec INTO recorded_digest,spec FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE r.report_id=NEW.report_id;
  IF recorded_digest IS DISTINCT FROM NEW.report_revision OR NEW.report_binding IS DISTINCT FROM jsonb_build_object('coverage','recorded_legacy_native_only','report_id',NEW.report_id,'report_revision',NEW.report_revision,'project_id',NEW.project_id,'task_id',spec#>'{task,task_id}','attempt_id',spec#>'{lifecycle,attempt_id}','source',jsonb_build_object('repository_id',spec#>'{source,repository_id}','base',spec#>'{source,resolved_object}')) OR NEW.project_id::TEXT IS DISTINCT FROM spec#>>'{lifecycle,project_id}' THEN RAISE EXCEPTION 'media exact report binding mismatch'; END IF;
  charge_reserved:=NEW.reserved_bytes;charge_retained:=0;history_delta:=1;
 END IF;
 -- All writers lock installation then project budgets before actual result
 -- authority. The Rust owner takes these same rows before load_locked.
 project_key:='project/'||NEW.project_id::TEXT;
 PERFORM 1 FROM result_media_budgets WHERE scope_key='installation' FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'media installation budget missing'; END IF;
 INSERT INTO result_media_budgets(scope_key,limit_bytes) VALUES(project_key,67108864) ON CONFLICT DO NOTHING;
 PERFORM 1 FROM result_media_budgets WHERE scope_key=project_key FOR UPDATE;
 IF TG_OP='INSERT' THEN
  SELECT count(*) INTO count_subject FROM result_media_intents WHERE report_id=NEW.report_id AND report_revision=NEW.report_revision AND state IN ('reserved','received','finalizing','retained');
  IF count_subject>=8 THEN RAISE EXCEPTION 'media report attachment capacity'; END IF;
 END IF;
 IF TG_OP='INSERT' OR NEW.state='retained' AND OLD.state<>'retained' THEN
  SELECT role,jsonb_build_object('issuer',issuer,'subject',subject) INTO role_name,current_actor FROM users WHERE id=NEW.actor_id FOR SHARE;
  IF role_name IS DISTINCT FROM 'owner' OR current_actor IS DISTINCT FROM NEW.actor_identity OR jsonb_typeof(current_actor->'issuer') IS DISTINCT FROM 'string' OR jsonb_typeof(current_actor->'subject') IS DISTINCT FROM 'string' OR current_actor->>'issuer'='' OR current_actor->>'subject'='' THEN RAISE EXCEPTION 'media current human authority changed'; END IF;
 END IF;
 IF NEW.receipt_bytes IS NOT NULL THEN
  receipt:=convert_from(NEW.receipt_bytes,'UTF8')::JSONB;
  IF jsonb_typeof(receipt) IS DISTINCT FROM 'object' OR NOT receipt ?& ARRAY['schema_version','artifact_id','binding','uploader_id','input','capture','rendering_policy','variants','retained_at_ms','retain_until_ms'] OR receipt-ARRAY['schema_version','artifact_id','binding','uploader_id','input','capture','rendering_policy','variants','retained_at_ms','retain_until_ms']<>'{}'::JSONB THEN RAISE EXCEPTION 'invalid media receipt object'; END IF;
  IF receipt->'schema_version' IS DISTINCT FROM '1'::JSONB OR receipt->>'artifact_id' IS DISTINCT FROM NEW.operation_id::TEXT OR receipt->>'uploader_id' IS DISTINCT FROM NEW.actor_id::TEXT OR receipt->'binding' IS DISTINCT FROM NEW.report_binding OR receipt->'input' IS DISTINCT FROM jsonb_build_object('digest',NEW.input_digest,'bytes',NEW.input_bytes,'filename',NEW.filename) OR receipt->'capture' IS DISTINCT FROM NEW.capture OR receipt->>'rendering_policy' IS DISTINCT FROM NEW.rendering_policy OR jsonb_typeof(receipt->'retain_until_ms') IS DISTINCT FROM 'number' OR jsonb_typeof(receipt->'retained_at_ms') IS DISTINCT FROM 'number' OR (receipt->>'retain_until_ms')::BIGINT IS DISTINCT FROM NEW.retain_until_ms OR (receipt->>'retained_at_ms')::BIGINT<NEW.created_at_ms OR (receipt->>'retained_at_ms')::BIGINT>=NEW.upload_expires_at_ms THEN RAISE EXCEPTION 'media receipt identity mismatch'; END IF;
  logical_bytes:=NEW.input_bytes+(receipt#>>'{variants,render,bytes}')::BIGINT+(receipt#>>'{variants,preview,bytes}')::BIGINT;
  IF jsonb_typeof(receipt->'variants') IS DISTINCT FROM 'object' OR NOT (receipt->'variants') ?& ARRAY['render','preview'] OR (receipt->'variants')-ARRAY['render','preview']<>'{}'::JSONB THEN RAISE EXCEPTION 'invalid media variants'; END IF;
  FOREACH variant IN ARRAY ARRAY[receipt#>'{variants,render}',receipt#>'{variants,preview}'] LOOP
   IF jsonb_typeof(variant) IS DISTINCT FROM 'object' OR NOT variant ?& ARRAY['digest','bytes','mime','width','height'] OR variant-ARRAY['digest','bytes','mime','width','height']<>'{}'::JSONB OR jsonb_typeof(variant->'digest') IS DISTINCT FROM 'string' OR jsonb_typeof(variant->'bytes') IS DISTINCT FROM 'number' OR jsonb_typeof(variant->'width') IS DISTINCT FROM 'number' OR jsonb_typeof(variant->'height') IS DISTINCT FROM 'number' OR variant->>'mime' IS DISTINCT FROM 'image/png' OR (variant->>'digest') !~ '^sha256:[0-9a-f]{64}$' OR (variant->>'bytes')::BIGINT NOT BETWEEN 1 AND 8388608 OR (variant->>'width')::BIGINT NOT BETWEEN 1 AND 4096 OR (variant->>'height')::BIGINT NOT BETWEEN 1 AND 4096 OR (variant->>'width')::BIGINT*(variant->>'height')::BIGINT>2000000 THEN RAISE EXCEPTION 'invalid media variant descriptor'; END IF;
  END LOOP;
  render:=receipt#>'{variants,render}';preview:=receipt#>'{variants,preview}';
  IF (preview->>'bytes')::BIGINT>786432 OR (preview->>'width')::BIGINT<>GREATEST(1,(render->>'width')::BIGINT*LEAST(384,GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT))/GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT)) OR (preview->>'height')::BIGINT<>GREATEST(1,(render->>'height')::BIGINT*LEAST(384,GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT))/GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT)) THEN RAISE EXCEPTION 'invalid media preview bounds'; END IF;
  IF NEW.cleanup_state<>'done' AND (NEW.retained_bytes<logical_bytes OR NEW.retained_bytes>10485760) THEN RAISE EXCEPTION 'media physical charge below retained bytes'; END IF;
 END IF;
 UPDATE result_media_budgets SET reserved_bytes=reserved_bytes+charge_reserved,retained_bytes=retained_bytes+charge_retained,historical_intents=historical_intents+history_delta WHERE scope_key IN ('installation',project_key);
 RETURN NEW;
END $$;
CREATE TRIGGER result_media_intent_insert_guard BEFORE INSERT ON result_media_intents FOR EACH ROW EXECUTE FUNCTION guard_result_media_intent();
CREATE TRIGGER result_media_intent_update_guard BEFORE UPDATE ON result_media_intents FOR EACH ROW EXECUTE FUNCTION guard_result_media_intent();
CREATE TRIGGER result_media_intent_delete_guard BEFORE DELETE ON result_media_intents FOR EACH ROW EXECUTE FUNCTION guard_result_media_intent();
CREATE TRIGGER result_media_intent_truncate_guard BEFORE TRUNCATE ON result_media_intents FOR EACH STATEMENT EXECUTE FUNCTION guard_result_media_intent();
UPDATE meta SET value='36' WHERE key='state_format_version';
