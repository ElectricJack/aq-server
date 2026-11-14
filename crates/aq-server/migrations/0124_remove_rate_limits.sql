-- Spec 0008: the rate limits are removed (owner, 2026-11-13). Offers: "lets just remove
-- them instead"; worker reports: "Yes remove the worker report limit too". Growth of
-- sealed history and of the report log is bounded by deleting old records (architecture
-- 0006), and a task that fails in a loop by its retry, lineage, time and cost budgets.
-- Gone: the offer rows of sealed_history_limits() (30 per node, 240 per project, 960 per
-- server an hour) and the fence 0105 put on profiled_launches; the report rows (720 per
-- node, 11,520 per server an hour) and the rate branch of the 0104 presence guard. The
-- live-row caps stay, including the presence guard's live cap.
-- A lock that is not granted in 5 s fails this migration (55P03) instead of hanging
-- startup: the whole migration transaction rolls back and the server exits, to retry.
SET LOCAL lock_timeout = '5s';
CREATE OR REPLACE FUNCTION sealed_history_limits()
RETURNS TABLE(kind TEXT, scope TEXT, max_rows BIGINT, max_bytes BIGINT, headroom_rows BIGINT, headroom_bytes BIGINT)
LANGUAGE sql IMMUTABLE AS $$
 SELECT v.k,v.s,v.r::BIGINT,v.b::BIGINT,v.hr::BIGINT,v.hb::BIGINT FROM (VALUES
  ('admission_artifacts','global',4096,268435456,64,4194304),
  ('admission_artifacts','project',512,67108864,64,4194304),
  ('intelligence_decisions','global',1024,67108864,2,16777216),
  ('launch_registrations','global',4096,262144,1,64),
  ('launch_registrations','node',256,16384,1,64),
  ('native_preparation','global',4096,67108864,1,262144),
  ('native_preparation','node',256,4194304,1,262144),
  ('phase_terminal','global',256,268435456,1,1048576),
  ('phase_terminal','node',16,16777216,1,1048576),
  ('presence_operations','global',65536,268435456,1,0),
  ('presence_operations','node',4096,16777216,1,0),
  ('source_exports','global',4096,268435456,1,4202496),
  ('source_exports','project',256,67108864,1,4202496),
  ('v2_proposals','global',4096,NULL,1,0),
  ('v2_proposals','node',256,NULL,1,0)
 ) v(k,s,r,b,hr,hb)
$$;
CREATE OR REPLACE FUNCTION sealed_history_scoped(p_kind TEXT, p_now_ms BIGINT, p_exclude UUID DEFAULT NULL, p_live UUID[] DEFAULT NULL)
RETURNS TABLE(s_scope UUID, s_rows BIGINT, s_bytes BIGINT)
LANGUAGE plpgsql STABLE AS $$
DECLARE live UUID[] := COALESCE(p_live,(SELECT array_agg(v.attempt_id) FROM sealed_live_attempt_ids v),'{}');
BEGIN
 IF p_kind='admission_artifacts' THEN
  RETURN QUERY WITH ids AS (
    SELECT l.artifact_id FROM profiled_launch_artifacts l WHERE l.attempt_id=ANY(live)
    UNION
    SELECT a.artifact_id FROM admission_artifacts a WHERE a.created_at_ms>=p_now_ms-600000
     AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id))
   SELECT CASE WHEN a.owner_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' THEN a.owner_key::UUID END,
    count(*)::BIGINT,COALESCE(sum(octet_length(a.body)),0)::BIGINT
   FROM admission_artifacts a WHERE a.artifact_id=ANY(ARRAY(SELECT ids.artifact_id FROM ids)) GROUP BY 1;
 ELSIF p_kind='intelligence_decisions' THEN
  RETURN QUERY WITH ids AS (
    SELECT l.decision_id FROM profiled_launches l WHERE l.attempt_id=ANY(live)
    UNION
    SELECT d.decision_id FROM intelligence_decisions d WHERE (d.decision#>>'{inputs,now_ms}')::BIGINT>=p_now_ms-600000
     AND NOT EXISTS(SELECT 1 FROM profiled_launches l WHERE l.decision_id=d.decision_id))
   SELECT NULL::UUID,count(*)::BIGINT,COALESCE(sum(pg_column_size(d.decision)),0)::BIGINT
   FROM intelligence_decisions d WHERE d.decision_id=ANY(ARRAY(SELECT ids.decision_id FROM ids));
 ELSIF p_kind IN ('source_exports','staging_exports') THEN
  RETURN QUERY WITH ids AS (
    SELECT e.operation_id FROM profiled_source_exports e WHERE e.state='pending'
    UNION
    SELECT l.operation_id FROM profiled_launch_source_exports l WHERE l.attempt_id=ANY(live)
    UNION
    SELECT b.profiled_export_operation_id FROM source_bases b WHERE b.attempt_id=ANY(live)
     AND b.profiled_export_operation_id IS NOT NULL
    UNION
    SELECT e.operation_id FROM profiled_source_exports e WHERE p_now_ms>0 AND e.created_at_ms>=p_now_ms-600000
     AND NOT EXISTS(SELECT 1 FROM profiled_launch_source_exports l WHERE l.operation_id=e.operation_id)
     AND NOT EXISTS(SELECT 1 FROM source_bases b WHERE b.profiled_export_operation_id=e.operation_id))
   SELECT CASE WHEN p_kind='staging_exports' THEN NULL ELSE e.project_id END,count(*)::BIGINT,
    COALESCE(sum(CASE WHEN p_kind='staging_exports' THEN e.charged_bytes
     ELSE octet_length(e.request_bytes)+COALESCE(octet_length(e.verified_bytes),4194304) END),0)::BIGINT
   FROM profiled_source_exports e WHERE e.operation_id=ANY(ARRAY(SELECT ids.operation_id FROM ids))
   AND e.operation_id IS DISTINCT FROM p_exclude GROUP BY 1;
 ELSIF p_kind='native_preparation' THEN
  RETURN QUERY WITH ls AS (
    SELECT m.lease_id,m.node_id,octet_length(m.lease::text)::BIGINT AS bytes FROM admission_material_leases m WHERE m.state='pending'
    UNION
    SELECT m.lease_id,m.node_id,octet_length(m.lease::text)::BIGINT FROM admission_material_leases m
     WHERE m.prospective_attempt_id=ANY(live)),
   charged AS (
    SELECT l.node_id,l.bytes FROM ls l
    UNION ALL SELECT r.node_id,octet_length(r.response::text)::BIGINT FROM profiled_execution_receipts r
    UNION ALL SELECT l.node_id,octet_length(mr.receipt::text)::BIGINT FROM admission_material_receipts mr JOIN ls l ON l.lease_id=mr.lease_id
     WHERE mr.lease_id=ANY(ARRAY(SELECT x.lease_id FROM ls x))
    UNION ALL SELECT p.node_id,(octet_length(p.proposal::text)+octet_length(p.policy::text))::BIGINT
     FROM profiled_v2_proposals p WHERE p.attempt_id=ANY(live)
    UNION ALL SELECT a.node_id,(octet_length(a.request::text)+octet_length(a.receipt::text))::BIGINT
     FROM profiled_v2_accepts a WHERE a.attempt_id=ANY(live)
    UNION ALL SELECT g.node_id,octet_length(g.request_bytes)::BIGINT+octet_length(g.receipt_bytes)+octet_length(g.provenance_bytes)
     FROM profiled_phase_execution_grants g WHERE g.attempt_id=ANY(live))
   SELECT COALESCE(a.node_id,b.node_id),COALESCE(a.n,0)::BIGINT,COALESCE(b.s,0)::BIGINT
   FROM (SELECT l.node_id,count(*) AS n FROM ls l GROUP BY l.node_id) a
   FULL JOIN (SELECT c.node_id,sum(c.bytes) AS s FROM charged c GROUP BY c.node_id) b ON a.node_id=b.node_id;
 ELSIF p_kind='v2_proposals' THEN
  RETURN QUERY SELECT p.node_id,count(*)::BIGINT,0::BIGINT
   FROM profiled_v2_proposals p WHERE p.attempt_id=ANY(live) GROUP BY p.node_id;
 ELSIF p_kind='launch_registrations' THEN
  RETURN QUERY SELECT r.node_id,count(*)::BIGINT,COALESCE(sum(r.logical_bytes),0)::BIGINT
   FROM profiled_launch_registrations r WHERE r.attempt_id=ANY(live) GROUP BY r.node_id;
 ELSIF p_kind='phase_terminal' THEN
  RETURN QUERY SELECT c.node_id,count(*)::BIGINT,COALESCE(sum(c.reserved_bytes),0)::BIGINT
   FROM profiled_phase_terminal_capacity c WHERE c.attempt_id=ANY(live) GROUP BY c.node_id;
 ELSIF p_kind='presence_operations' THEN
  RETURN QUERY SELECT o.node_id,count(*)::BIGINT,COALESCE(sum(octet_length(o.request_bytes)+octet_length(o.receipt_bytes)+1024),0)::BIGINT
   FROM profiled_presence_operations o
   WHERE o.expires_at_ms>p_now_ms OR (o.attempt_id=ANY(live) AND o.kind='presence') GROUP BY o.node_id;
 ELSIF p_kind IN ('presence_rate','offers','project_offers') THEN
  -- Removed by 0124: no report or offer rate. Empty kinds, so a 0105 fence that the
  -- migrating role could not drop counts no offers and never refuses.
  RETURN;
 ELSE
  RAISE EXCEPTION 'unknown sealed history kind %',p_kind;
 END IF;
END $$;
-- The 0104 guard without its report rate: the body checks and the live-row cap are kept
-- byte for byte, and the storage counters still saturate as statistics.
CREATE OR REPLACE FUNCTION profiled_presence_operation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE g profiled_presence_storage%ROWTYPE; n profiled_presence_node_storage%ROWTYPE; incoming BIGINT; q JSONB; a JSONB; u RECORD;
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
 IF u.total_rows>=65536 OR u.scope_rows>=4096 OR u.total_bytes+incoming>268435456 OR u.scope_bytes+incoming>16777216
 THEN RAISE EXCEPTION 'scoped presence retention capacity' USING ERRCODE='54000'; END IF;
 UPDATE profiled_presence_storage SET operations=LEAST(operations+1,65536),logical_bytes=LEAST(logical_bytes+incoming,268435456) WHERE singleton;
 UPDATE profiled_presence_node_storage SET operations=LEAST(operations+1,4096),logical_bytes=LEAST(logical_bytes+incoming,16777216) WHERE node_id=NEW.node_id;
 RETURN NEW;
END $$;
-- 0105 created the trigger and its function as the migrating role, on a table that
-- role created (0010), so this role can drop both. A role that cannot drop the trigger
-- leaves it: the fence then counts no offers (above), but its advisory lock still
-- serialises launch inserts, so a function the role owns is emptied. This migration
-- applies either way.
DO $$
DECLARE fence REGPROCEDURE := to_regprocedure('profiled_launch_offer_rate()');
 owns_fence BOOLEAN;
BEGIN
 owns_fence := fence IS NOT NULL AND pg_has_role((SELECT p.proowner FROM pg_proc p WHERE p.oid=fence),'USAGE');
 IF EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=to_regclass('profiled_launches')
   AND t.tgname='profiled_launch_offer_rate' AND NOT t.tgisinternal) THEN
  IF pg_has_role((SELECT c.relowner FROM pg_class c WHERE c.oid=to_regclass('profiled_launches')),'USAGE') THEN
   DROP TRIGGER profiled_launch_offer_rate ON profiled_launches;
  ELSIF owns_fence THEN
   EXECUTE 'CREATE OR REPLACE FUNCTION profiled_launch_offer_rate() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN RETURN NEW; END $f$';
   RAISE WARNING 'offer rate fence left on profiled_launches: % does not own the table; its function was emptied', current_user;
  ELSE
   RAISE WARNING 'offer rate fence left on profiled_launches: % owns neither the table nor its function; it counts no offers but still serialises launch inserts', current_user;
  END IF;
 END IF;
 IF owns_fence AND NOT EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgfoid=fence) THEN
  DROP FUNCTION profiled_launch_offer_rate();
 END IF;
END $$;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='124' WHERE key='state_format_version';
