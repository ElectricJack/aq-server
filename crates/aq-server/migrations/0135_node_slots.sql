-- One server-owned count per enrolled node, shared by native and manual work.
CREATE TABLE node_slot_default (
 singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
 slots INTEGER NOT NULL DEFAULT 2 CHECK (slots BETWEEN 0 AND 1024),
 revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0)
);
INSERT INTO node_slot_default DEFAULT VALUES;
INSERT INTO worker_settings(kind,id,max_agents,revision)
 SELECT 'node',id::text,1,1 FROM nodes
 ON CONFLICT(kind,id) DO UPDATE SET
 max_agents=LEAST(COALESCE(worker_settings.max_agents,1),1),
 revision=worker_settings.revision+1;
CREATE FUNCTION initialize_node_slots() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO worker_settings(kind,id,max_agents,revision)
 SELECT 'node',NEW.id::text,slots,1 FROM node_slot_default;
 RETURN NEW;
END $$;
CREATE TRIGGER initialize_node_slots AFTER INSERT ON nodes
 FOR EACH ROW EXECUTE FUNCTION initialize_node_slots();

-- Attempts are sealed; allocation is kept separately for their whole lifetime.
CREATE TABLE execution_attempt_slots (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 node_id UUID NOT NULL,
 slot INTEGER NOT NULL CHECK (slot BETWEEN 1 AND 1024)
);
CREATE INDEX execution_attempt_slots_node ON execution_attempt_slots(node_id,slot);
INSERT INTO execution_attempt_slots SELECT attempt_id,node_id,1 FROM execution_attempts;
ALTER TABLE manual_project_slots DROP CONSTRAINT manual_project_slots_slots_check;
ALTER TABLE manual_project_slots ADD CHECK (slots BETWEEN 0 AND 1024);
CREATE TABLE manual_agent_slots (
 session_id UUID PRIMARY KEY REFERENCES manual_agent_sessions(id) ON DELETE CASCADE,
 node_id UUID NOT NULL REFERENCES nodes(id),
 slot INTEGER NOT NULL CHECK (slot BETWEEN 1 AND 1024)
);
-- Retain every existing live session; migration never cancels execution.
INSERT INTO manual_agent_slots
 SELECT id,node_id,(row_number() OVER (PARTITION BY node_id ORDER BY created_at_ms,id))::INTEGER
 FROM manual_agent_sessions WHERE stopped_at_ms IS NULL AND status<>'pending';

-- Exact SQL proof shared by admission, reads and incarnation fencing. A released
-- task is already guarded by its settlement owner; expiry/liveness is no proof.
CREATE FUNCTION native_attempt_holds_compute(attempt UUID, owner_node UUID)
RETURNS BOOLEAN LANGUAGE plpgsql STABLE AS $$
DECLARE held BOOLEAN; attempt_kind TEXT; status TEXT;
BEGIN
 SELECT a.holds_task,a.kind,h.state->>'status' INTO held,attempt_kind,status
 FROM execution_attempts a LEFT JOIN profiled_authority_hot h USING(attempt_id)
 WHERE a.attempt_id=attempt AND a.node_id=owner_node;
 IF NOT FOUND OR NOT held THEN RETURN FALSE; END IF;
 -- Neither stopped proof can be valid outside Finished. Avoid planning the
 -- historical proof joins on every active-attempt count in the Swarm read.
 IF attempt_kind<>'profiled_v1' OR status IS DISTINCT FROM 'finished' THEN RETURN TRUE; END IF;
 RETURN NOT profiled_compute_stopped_valid(attempt,owner_node)
   AND NOT EXISTS(SELECT 1 FROM lost_attempt_releases l
    JOIN profiled_authority_hot h USING(attempt_id)
    WHERE l.attempt_id=attempt AND l.node_id=owner_node
     AND h.state->>'status'='finished'
     AND h.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object(
      'id','aq.lost-attempt-release/'||attempt::text,'version','1','digest',l.receipt_digest)));
END $$;

-- Preserve the typed Rust stop-validation witness of the original fence.
CREATE FUNCTION native_compute_reuse_proven(attempt UUID, owner_node UUID)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT NOT native_attempt_holds_compute(attempt,owner_node)
 AND (NOT EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=attempt AND holds_task)
  OR EXISTS(SELECT 1 FROM lost_attempt_releases l JOIN profiled_authority_hot h USING(attempt_id)
   WHERE l.attempt_id=attempt AND l.node_id=owner_node AND h.state->>'status'='finished'
    AND h.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object(
     'id','aq.lost-attempt-release/'||attempt::text,'version','1','digest',l.receipt_digest)))
  OR EXISTS(SELECT 1 FROM profiled_phase_execution_grants g
   JOIN profiled_stopped_receipts s USING(attempt_id) JOIN profiled_v2_proposals p USING(attempt_id)
   WHERE g.attempt_id=attempt AND profiled_compute_stopped_valid(attempt,owner_node)
    AND COALESCE(NULLIF(current_setting('aq.compute_stopped',true),'')::JSONB,'{}'::JSONB)->attempt::text
     =jsonb_build_object('kind',CASE p.proposal#>>'{negotiation,contract,id}'
      WHEN 'aq.admission.preparation-v2-contract' THEN 'phase_execution_v2'
      WHEN 'aq.admission.preparation-v2-contract.interactive-long-v1' THEN 'phase_execution_v2'
      WHEN 'aq.admission.preparation-registry-refresh-v1-contract' THEN 'phase_refresh_execution_v1'
      WHEN 'aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1' THEN 'phase_refresh_execution_v1'
      ELSE 'unsupported' END,'execution',g.receipt_digest,'stopped_ack',s.phase_ack_digest)))
$$;

CREATE FUNCTION node_slot_assignment_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE owner UUID; index_value INTEGER; maximum INTEGER; excluded_attempt UUID; excluded_session UUID;
BEGIN
 IF TG_OP='DELETE' THEN
  IF TG_TABLE_NAME='execution_attempt_slots' THEN
   IF EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=OLD.attempt_id) THEN
    RAISE EXCEPTION 'slot assignments are immutable';
   END IF;
  ELSE
   IF EXISTS(SELECT 1 FROM manual_agent_sessions WHERE id=OLD.session_id) THEN
    RAISE EXCEPTION 'slot assignments are immutable';
   END IF;
  END IF;
  RETURN OLD;
 END IF;
 IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'slot assignments are immutable'; END IF;
 owner:=NEW.node_id; index_value:=NEW.slot;
 -- Same lock order as management/manual/queued publication. SQL also fences
 -- direct writes; SERIALIZABLE publication retries stale snapshots.
 PERFORM pg_advisory_xact_lock(110110);
 PERFORM 1 FROM nodes WHERE id=owner FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'slot node does not exist'; END IF;
 SELECT COALESCE(s.max_agents,d.slots) INTO maximum FROM node_slot_default d
 LEFT JOIN worker_settings s ON s.kind='node' AND s.id=owner::text;
 IF index_value>maximum THEN RAISE EXCEPTION 'node slot count reached'; END IF;
 IF TG_TABLE_NAME='execution_attempt_slots' THEN
  excluded_attempt:=NEW.attempt_id;
  IF NOT EXISTS(SELECT 1 FROM execution_attempts a WHERE a.attempt_id=excluded_attempt AND a.node_id=owner)
  THEN RAISE EXCEPTION 'slot attempt scope mismatch'; END IF;
 ELSE
  excluded_session:=NEW.session_id;
  IF NOT EXISTS(SELECT 1 FROM manual_agent_sessions m WHERE m.id=excluded_session AND m.node_id=owner)
  THEN RAISE EXCEPTION 'slot session scope mismatch'; END IF;
 END IF;
 IF EXISTS(SELECT 1 FROM execution_attempts a LEFT JOIN execution_attempt_slots s USING(attempt_id)
  WHERE a.node_id=owner AND a.attempt_id IS DISTINCT FROM excluded_attempt AND a.holds_task
   AND (a.kind<>'profiled_v1' OR s.slot IS NULL OR s.slot=index_value)
   AND NOT native_compute_reuse_proven(a.attempt_id,owner))
 OR EXISTS(SELECT 1 FROM manual_agent_sessions m LEFT JOIN manual_agent_slots s ON s.session_id=m.id
  WHERE m.node_id=owner AND m.id IS DISTINCT FROM excluded_session
   AND m.stopped_at_ms IS NULL AND m.status<>'pending' AND (s.slot IS NULL OR s.slot=index_value))
 THEN RAISE EXCEPTION 'node slot remains occupied'; END IF;
 IF (SELECT count(*) FROM execution_attempts a WHERE a.node_id=owner
      AND a.attempt_id IS DISTINCT FROM excluded_attempt AND native_attempt_holds_compute(a.attempt_id,owner))
  +(SELECT count(*) FROM manual_agent_sessions m WHERE m.node_id=owner
      AND m.id IS DISTINCT FROM excluded_session AND m.stopped_at_ms IS NULL AND m.status<>'pending')>=maximum
 THEN RAISE EXCEPTION 'node capacity reached'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER execution_attempt_slot_guard BEFORE INSERT OR UPDATE OR DELETE ON execution_attempt_slots
 FOR EACH ROW EXECUTE FUNCTION node_slot_assignment_guard();
CREATE TRIGGER manual_agent_slot_guard BEFORE INSERT OR UPDATE OR DELETE ON manual_agent_slots
 FOR EACH ROW EXECUTE FUNCTION node_slot_assignment_guard();

CREATE OR REPLACE FUNCTION native_incarnation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND OLD.state->'native' IS NOT NULL AND OLD.state->'native'<>'null'::jsonb
    AND (NEW.state->'native' IS NULL OR NEW.state->'native'='null'::jsonb) THEN
  RAISE EXCEPTION 'native incarnation cannot become a fixture registration';
 END IF;
 IF NEW.state->'native' IS NOT NULL AND NEW.state->'native'<>'null'::jsonb THEN
  IF octet_length(NEW.state::text)>65536 OR NEW.state->'identity' IS DISTINCT FROM NEW.state#>'{native,request,identity}' THEN
   RAISE EXCEPTION 'invalid native incarnation metadata';
  END IF;
  IF TG_OP='INSERT' OR NEW.state IS DISTINCT FROM OLD.state THEN
   IF EXISTS(SELECT 1 FROM execution_attempts a LEFT JOIN execution_attempt_slots s USING(attempt_id)
    WHERE a.node_id=NEW.node_id AND a.holds_task AND NOT native_compute_reuse_proven(a.attempt_id,NEW.node_id)
     AND NOT COALESCE((jsonb_typeof(NEW.state->'slots')='number' AND (NEW.state->>'slots')::INTEGER BETWEEN 1 AND 1024
      AND a.kind='profiled_v1' AND s.node_id=NEW.node_id AND s.slot IS NOT NULL),FALSE)) THEN
    RAISE EXCEPTION 'execution claims fence native incarnation changes';
   END IF;
  END IF;
 END IF;
 RETURN NEW;
END $$;
UPDATE meta SET value='135' WHERE key='state_format_version';
