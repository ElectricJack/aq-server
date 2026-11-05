-- Result media clips (X26): a bounded ISO-BMFF video/mp4 clip retained
-- byte-for-byte under aq.mp4.passthrough-v1, with a retained PNG poster whose
-- preview is the clip's preview. Image intents and receipts are unchanged.
DO $$
DECLARE c RECORD;
BEGIN
 FOR c IN SELECT con.conname FROM pg_constraint con WHERE con.conrelid='result_media_intents'::regclass AND con.contype='c'
   AND con.conkey IN (SELECT ARRAY[a.attnum] FROM pg_attribute a WHERE a.attrelid=con.conrelid AND a.attname IN ('rendering_policy','input_bytes','rejection')) LOOP
  EXECUTE format('ALTER TABLE result_media_intents DROP CONSTRAINT %I',c.conname);
 END LOOP;
END $$;
ALTER TABLE result_media_intents ADD CONSTRAINT result_media_intents_rendering_policy_check CHECK(rendering_policy IN ('aq.png.rgba8-srgb-v1','aq.mp4.passthrough-v1'));
ALTER TABLE result_media_intents ADD CONSTRAINT result_media_intents_input_bytes_check CHECK(input_bytes BETWEEN 1 AND CASE WHEN rendering_policy='aq.mp4.passthrough-v1' THEN 4194304 ELSE 1048576 END);
ALTER TABLE result_media_intents ADD CONSTRAINT result_media_intents_rejection_check CHECK(rejection IN ('invalid_png','unsupported_png','input_mismatch','pixel_limit','processing_limit','storage_failure','invalid_video','unsupported_video','poster_unavailable'));

CREATE OR REPLACE FUNCTION guard_result_media_intent() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE request JSONB; receipt JSONB; variant JSONB; render JSONB; preview JSONB; spec JSONB; recorded_digest TEXT; current_actor JSONB; role_name TEXT; clip BOOLEAN:=NEW.rendering_policy='aq.mp4.passthrough-v1';
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
  IF OLD.state='rejected' AND NEW.rejection IS DISTINCT FROM OLD.rejection THEN RAISE EXCEPTION 'terminal media rejection is immutable'; END IF;
  IF OLD.state='received' AND NEW.state='reserved' OR OLD.state='finalizing' AND NEW.state IN ('reserved','received') THEN RAISE EXCEPTION 'media state cannot regress'; END IF;
  IF OLD.cleanup_state='done' AND NEW IS DISTINCT FROM OLD THEN RAISE EXCEPTION 'completed media cleanup is immutable'; END IF;
  IF OLD.cleanup_state='none' AND NEW.cleanup_state NOT IN ('none','running') OR OLD.cleanup_state='running' AND NEW.cleanup_state NOT IN ('running','blocked','done') OR OLD.cleanup_state='blocked' AND NEW.cleanup_state NOT IN ('blocked','running','done') THEN RAISE EXCEPTION 'media cleanup cannot regress'; END IF;
  IF NEW.lease_id IS NOT NULL AND (OLD.lease_id IS NULL OR NEW.lease_id<>OLD.lease_id) AND (NEW.lease_expires_at_ms<=NEW.transition_at_ms OR NEW.cleanup_state<>'none') THEN RAISE EXCEPTION 'media new lease is not live'; END IF;
  IF NEW.lease_id IS NOT NULL AND NEW.lease_id=OLD.lease_id AND NEW.lease_expires_at_ms IS DISTINCT FROM OLD.lease_expires_at_ms THEN RAISE EXCEPTION 'media lease deadline is immutable'; END IF;
  IF NEW.lease_id IS NOT NULL AND OLD.lease_id IS NOT NULL AND NEW.lease_id<>OLD.lease_id AND NEW.transition_at_ms<OLD.lease_expires_at_ms THEN RAISE EXCEPTION 'live media lease cannot be replaced'; END IF;
  IF NEW.processing_attempts>OLD.processing_attempts AND (NEW.state<>'finalizing' OR NEW.lease_id IS NULL OR NEW.lease_id IS NOT DISTINCT FROM OLD.lease_id) THEN RAISE EXCEPTION 'media processing needs a new fenced lease'; END IF;
  IF NEW.state='finalizing' AND NEW.cleanup_state='none' AND (OLD.state<>'finalizing' OR NEW.lease_id IS DISTINCT FROM OLD.lease_id OR NEW.processing_attempts<>OLD.processing_attempts) THEN
   IF NEW.lease_id IS NULL OR NEW.lease_id IS NOT DISTINCT FROM OLD.lease_id OR NEW.processing_attempts<>OLD.processing_attempts+1 OR NEW.transition_at_ms>=NEW.upload_expires_at_ms THEN RAISE EXCEPTION 'media finalizing entry needs original-deadline processing lease'; END IF;
  END IF;
  IF NEW.state='finalizing' AND NEW.cleanup_state='none' AND NEW.lease_id IS NULL THEN RAISE EXCEPTION 'media finalizing owner lease missing'; END IF;
  IF NEW.state='retained' AND OLD.state<>'retained' THEN
   IF OLD.state<>'finalizing' OR OLD.lease_id IS NULL OR OLD.processing_attempts<1 OR OLD.cleanup_state<>'none' OR NEW.cleanup_state<>'none' OR NEW.transition_at_ms>=OLD.lease_expires_at_ms OR NEW.transition_at_ms>=OLD.upload_expires_at_ms OR (convert_from(NEW.receipt_bytes,'UTF8')::JSONB->>'retained_at_ms')::BIGINT IS DISTINCT FROM NEW.transition_at_ms THEN RAISE EXCEPTION 'media retention needs live original processing lease'; END IF;
  END IF;
  IF NEW.cleanup_state IN ('running','done') AND NEW.cleanup_state<>OLD.cleanup_state THEN
   IF NEW.transition_at_ms<(CASE WHEN OLD.state='retained' THEN OLD.retain_until_ms WHEN OLD.state='rejected' THEN OLD.transition_at_ms ELSE OLD.upload_expires_at_ms END) OR NEW.lease_id IS NOT NULL THEN RAISE EXCEPTION 'media cleanup is premature'; END IF;
  END IF;
  IF NEW.reserved_bytes<OLD.reserved_bytes AND NOT (NEW.state='retained' AND OLD.state='finalizing' OR NEW.cleanup_state='done') THEN RAISE EXCEPTION 'media reservation cannot be discarded'; END IF;
  IF NEW.reserved_bytes>OLD.reserved_bytes THEN RAISE EXCEPTION 'media reservation cannot grow'; END IF;
  IF NEW.retained_bytes<OLD.retained_bytes AND NEW.cleanup_state<>'done' THEN RAISE EXCEPTION 'media retained charge needs confirmed cleanup'; END IF;
  IF NEW.retained_bytes>OLD.retained_bytes AND NOT (OLD.state='finalizing' AND NEW.state='retained' AND OLD.retained_bytes=0) THEN RAISE EXCEPTION 'media retained charge cannot grow'; END IF;
  charge_reserved:=NEW.reserved_bytes-OLD.reserved_bytes;charge_retained:=NEW.retained_bytes-OLD.retained_bytes;history_delta:=0;
 ELSE
  IF NEW.state<>'reserved' OR NEW.reserved_bytes<>10485760 OR NEW.retained_bytes<>0 OR NEW.processing_attempts<>0 OR NEW.lease_id IS NOT NULL OR NEW.cleanup_state<>'none' OR NEW.receipt_bytes IS NOT NULL OR NEW.transition_at_ms<>NEW.created_at_ms THEN RAISE EXCEPTION 'new media intent must reserve before IO'; END IF;
  request:=convert_from(NEW.request_bytes,'UTF8')::JSONB;
  IF jsonb_typeof(request)<>'object' OR NOT request ?& ARRAY['operation_id','input_digest','input_bytes','filename','capture'] OR request-ARRAY['operation_id','input_digest','input_bytes','filename','capture','media','poster_artifact_id']<>'{}'::JSONB
   OR (request ? 'media')<>clip OR (request ? 'poster_artifact_id')<>clip OR clip AND (request->>'media' IS DISTINCT FROM 'video' OR jsonb_typeof(request->'poster_artifact_id') IS DISTINCT FROM 'string' OR request->>'poster_artifact_id'=NEW.operation_id::TEXT)
   OR NEW.input_bytes>(CASE WHEN clip THEN 4194304 ELSE 1048576 END)
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
  IF jsonb_typeof(receipt) IS DISTINCT FROM 'object' OR NOT receipt ?& ARRAY['schema_version','artifact_id','binding','uploader_id','input','capture','rendering_policy','variants','retained_at_ms','retain_until_ms'] OR receipt-ARRAY['schema_version','artifact_id','binding','uploader_id','input','capture','rendering_policy','variants','retained_at_ms','retain_until_ms','clip']<>'{}'::JSONB THEN RAISE EXCEPTION 'invalid media receipt object'; END IF;
  IF (receipt ? 'clip')<>clip OR clip AND (jsonb_typeof(receipt->'clip') IS DISTINCT FROM 'object' OR NOT (receipt->'clip') ?& ARRAY['duration_ms','poster_artifact_id'] OR (receipt->'clip')-ARRAY['duration_ms','poster_artifact_id']<>'{}'::JSONB OR jsonb_typeof(receipt#>'{clip,duration_ms}') IS DISTINCT FROM 'number' OR (receipt#>>'{clip,duration_ms}')!~'^[1-9][0-9]{0,4}$' OR (receipt#>>'{clip,duration_ms}')::BIGINT>60000 OR receipt#>'{clip,poster_artifact_id}' IS DISTINCT FROM convert_from(NEW.request_bytes,'UTF8')::JSONB->'poster_artifact_id') THEN RAISE EXCEPTION 'invalid media clip receipt'; END IF;
  IF receipt->'schema_version' IS DISTINCT FROM '1'::JSONB OR receipt->>'artifact_id' IS DISTINCT FROM NEW.operation_id::TEXT OR receipt->>'uploader_id' IS DISTINCT FROM NEW.actor_id::TEXT OR receipt->'binding' IS DISTINCT FROM NEW.report_binding OR receipt->'input' IS DISTINCT FROM jsonb_build_object('digest',NEW.input_digest,'bytes',NEW.input_bytes,'filename',NEW.filename) OR receipt->'capture' IS DISTINCT FROM NEW.capture OR receipt->>'rendering_policy' IS DISTINCT FROM NEW.rendering_policy OR jsonb_typeof(receipt->'retain_until_ms') IS DISTINCT FROM 'number' OR jsonb_typeof(receipt->'retained_at_ms') IS DISTINCT FROM 'number' OR (receipt->>'retain_until_ms')::BIGINT IS DISTINCT FROM NEW.retain_until_ms OR (receipt->>'retained_at_ms')::BIGINT<NEW.created_at_ms OR (receipt->>'retained_at_ms')::BIGINT>=NEW.upload_expires_at_ms THEN RAISE EXCEPTION 'media receipt identity mismatch'; END IF;
  logical_bytes:=NEW.input_bytes+(receipt#>>'{variants,render,bytes}')::BIGINT+(receipt#>>'{variants,preview,bytes}')::BIGINT;
  IF jsonb_typeof(receipt->'variants') IS DISTINCT FROM 'object' OR NOT (receipt->'variants') ?& ARRAY['render','preview'] OR (receipt->'variants')-ARRAY['render','preview']<>'{}'::JSONB THEN RAISE EXCEPTION 'invalid media variants'; END IF;
  FOREACH variant IN ARRAY ARRAY[receipt#>'{variants,render}',receipt#>'{variants,preview}'] LOOP
   IF jsonb_typeof(variant) IS DISTINCT FROM 'object' OR NOT variant ?& ARRAY['digest','bytes','mime','width','height'] OR variant-ARRAY['digest','bytes','mime','width','height']<>'{}'::JSONB OR jsonb_typeof(variant->'digest') IS DISTINCT FROM 'string' OR jsonb_typeof(variant->'bytes') IS DISTINCT FROM 'number' OR jsonb_typeof(variant->'width') IS DISTINCT FROM 'number' OR jsonb_typeof(variant->'height') IS DISTINCT FROM 'number' OR variant->>'mime' NOT IN ('image/png','video/mp4') OR (variant->>'digest') !~ '^sha256:[0-9a-f]{64}$' OR (variant->>'bytes')::BIGINT NOT BETWEEN 1 AND 8388608 OR (variant->>'width')::BIGINT NOT BETWEEN 1 AND 4096 OR (variant->>'height')::BIGINT NOT BETWEEN 1 AND 4096 OR (variant->>'width')::BIGINT*(variant->>'height')::BIGINT>2000000 THEN RAISE EXCEPTION 'invalid media variant descriptor'; END IF;
  END LOOP;
  render:=receipt#>'{variants,render}';preview:=receipt#>'{variants,preview}';
  IF render->>'mime' IS DISTINCT FROM (CASE WHEN clip THEN 'video/mp4' ELSE 'image/png' END) OR preview->>'mime' IS DISTINCT FROM 'image/png' THEN RAISE EXCEPTION 'invalid media variant mime'; END IF;
  IF clip AND (render->>'digest' IS DISTINCT FROM NEW.input_digest OR (render->>'bytes')::BIGINT IS DISTINCT FROM NEW.input_bytes OR (preview->>'bytes')::BIGINT>786432 OR (preview->>'width')::BIGINT>384 OR (preview->>'height')::BIGINT>384) THEN RAISE EXCEPTION 'invalid media clip variants'; END IF;
  IF NOT clip AND ((preview->>'bytes')::BIGINT>786432 OR (preview->>'width')::BIGINT<>GREATEST(1,(render->>'width')::BIGINT*LEAST(384,GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT))/GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT)) OR (preview->>'height')::BIGINT<>GREATEST(1,(render->>'height')::BIGINT*LEAST(384,GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT))/GREATEST((render->>'width')::BIGINT,(render->>'height')::BIGINT))) THEN RAISE EXCEPTION 'invalid media preview bounds'; END IF;
  IF NEW.cleanup_state<>'done' AND (NEW.retained_bytes<logical_bytes OR NEW.retained_bytes>10485760) THEN RAISE EXCEPTION 'media physical charge below retained bytes'; END IF;
 END IF;
 UPDATE result_media_budgets SET reserved_bytes=reserved_bytes+charge_reserved,retained_bytes=retained_bytes+charge_retained,historical_intents=historical_intents+history_delta WHERE scope_key IN ('installation',project_key);
 RETURN NEW;
END $$;

-- Capture requirements may ask for a clip (kind=video) as well as an image.
CREATE OR REPLACE FUNCTION validate_result_task_policy() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r jsonb; ids text[] := ARRAY[]::text[]; s jsonb;
BEGIN
 FOR r IN SELECT * FROM jsonb_array_elements(NEW.requirements) LOOP
  IF jsonb_typeof(r)<>'object' OR (SELECT count(*) FROM jsonb_object_keys(r))<>11
   OR NOT (r ?& ARRAY['id','policy_revision','kind','scenario','steps','viewport','environment','count','required','applicability','gate_binding'])
   OR jsonb_typeof(r->'id')<>'string' OR length(btrim(r->>'id')) NOT BETWEEN 1 AND 128 OR (r->>'id')=ANY(ids)
   OR r->>'policy_revision'<>NEW.revision::text OR jsonb_typeof(r->'policy_revision')<>'string'
   OR r->>'kind' NOT IN ('image','video') OR jsonb_typeof(r->'steps')<>'array' OR jsonb_array_length(r->'steps')>64
   OR jsonb_typeof(r->'count')<>'number' OR (r->>'count')!~'^[1-8]$'
   OR jsonb_typeof(r->'required')<>'boolean'
   OR jsonb_typeof(r->'viewport')<>'object' OR (SELECT count(*) FROM jsonb_object_keys(r->'viewport'))<>3
   OR NOT ((r->'viewport') ?& ARRAY['width_css','height_css','dpr_milli'])
   OR EXISTS(SELECT 1 FROM unnest(ARRAY['width_css','height_css','dpr_milli']) k WHERE jsonb_typeof(r->'viewport'->k)<>'number')
   OR (r#>>'{viewport,width_css}')!~'^[1-9][0-9]{0,4}$' OR (r#>>'{viewport,height_css}')!~'^[1-9][0-9]{0,4}$'
   OR (r#>>'{viewport,dpr_milli}')!~'^[1-9][0-9]{0,4}$'
   OR (r#>>'{viewport,width_css}')::bigint>4096 OR (r#>>'{viewport,height_css}')::bigint>4096
   OR (r#>>'{viewport,dpr_milli}')::bigint NOT BETWEEN 100 AND 8000
   OR octet_length(r->>'scenario')>128 OR octet_length(r->>'environment')>128
   OR EXISTS(SELECT 1 FROM unnest(ARRAY['scenario','environment','applicability','gate_binding']) k WHERE jsonb_typeof(r->k)<>'string' OR length(btrim(r->>k)) NOT BETWEEN 1 AND 2048)
  THEN RAISE EXCEPTION 'invalid capture requirement'; END IF;
  FOR s IN SELECT * FROM jsonb_array_elements(r->'steps') LOOP
   IF jsonb_typeof(s)<>'string' OR length(btrim(s#>>'{}')) NOT BETWEEN 1 AND 2048 THEN RAISE EXCEPTION 'invalid capture step'; END IF;
  END LOOP;
  ids:=array_append(ids,r->>'id');
 END LOOP;
 RETURN NEW;
END $$;
UPDATE meta SET value='75' WHERE key='state_format_version';
