-- While an offer is assembling or retrying publication, its pins keep the
-- exact artifacts charged. Closing one assembly preserves its bytes and
-- has no effect on another assembly or a committed live attempt.
SET LOCAL lock_timeout='5s';

ALTER TABLE admission_artifacts ADD COLUMN offer_staged BOOLEAN NOT NULL DEFAULT false;
CREATE TABLE admission_offer_stages (
 stage_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL UNIQUE,
 project_id UUID NOT NULL,
 expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>0),
 closed_at_ms BIGINT CHECK (closed_at_ms>0)
);
CREATE INDEX admission_offer_stages_live ON admission_offer_stages(expires_at_ms)
 WHERE closed_at_ms IS NULL;
CREATE TABLE admission_offer_stage_pins (
 stage_id UUID NOT NULL REFERENCES admission_offer_stages(stage_id),
 artifact_id UUID NOT NULL REFERENCES admission_artifacts(artifact_id),
 PRIMARY KEY(stage_id,artifact_id)
);
CREATE INDEX admission_offer_stage_pins_artifact ON admission_offer_stage_pins(artifact_id);

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
     AND NOT a.offer_staged
     AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id)
    UNION
    SELECT p.artifact_id FROM admission_offer_stages s JOIN admission_offer_stage_pins p USING(stage_id)
     WHERE s.closed_at_ms IS NULL AND s.expires_at_ms>p_now_ms)
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
   FROM profiled_phase_terminal_capacity c WHERE c.attempt_id=ANY(live)
    AND NOT COALESCE((SELECT true FROM profiled_stopped_receipts s
        WHERE s.attempt_id=c.attempt_id AND s.initial_authority_kind='phase_execution_v2'),false)
    AND NOT COALESCE((SELECT true FROM profiled_launch_closures l
        WHERE l.attempt_id=c.attempt_id AND l.initial_authority_kind='phase_execution_v2'),false)
   GROUP BY c.node_id;
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

-- Read committed visibility is required when charging a reused artifact.
-- The same lock serializes new artifact charges and other staging pins.
CREATE FUNCTION admission_offer_pin(p_stage UUID, p_artifact UUID, p_at BIGINT)
RETURNS BOOLEAN LANGUAGE plpgsql AS $$
DECLARE s admission_offer_stages%ROWTYPE; item admission_artifacts%ROWTYPE; u RECORD;
BEGIN
 IF current_setting('transaction_isolation') <> 'read committed' THEN
  RAISE EXCEPTION 'offer artifact pin needs current visibility';
 END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-inline-artifact-quota-v1',0));
 SELECT * INTO s FROM admission_offer_stages WHERE stage_id=p_stage FOR SHARE;
 IF NOT FOUND OR s.closed_at_ms IS NOT NULL OR s.expires_at_ms<=p_at THEN RETURN false; END IF;
 SELECT * INTO item FROM admission_artifacts WHERE artifact_id=p_artifact;
 IF NOT FOUND OR item.owner_key<>s.project_id::TEXT THEN RETURN false; END IF;

 -- Legacy IDs can still be reused. Their original accounting is unchanged.
 IF NOT item.offer_staged THEN RETURN true; END IF;

 IF NOT EXISTS(SELECT 1 FROM admission_offer_stage_pins p
     JOIN admission_offer_stages b USING(stage_id) WHERE p.artifact_id=p_artifact
       AND b.closed_at_ms IS NULL AND b.expires_at_ms>p_at)
   AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l
     JOIN sealed_live_attempt_ids live USING(attempt_id) WHERE l.artifact_id=p_artifact)
 THEN
  SELECT * INTO u FROM sealed_history_usage('admission_artifacts',s.project_id,p_at);
  IF u.total_rows>=4096 OR u.scope_rows>=512
   OR u.total_bytes+COALESCE(octet_length(item.body),0)>268435456
   OR u.scope_bytes+COALESCE(octet_length(item.body),0)>67108864
  THEN RETURN false; END IF;
 END IF;
 INSERT INTO admission_offer_stage_pins VALUES(p_stage,p_artifact) ON CONFLICT DO NOTHING;
 RETURN true;
END $$;

CREATE OR REPLACE FUNCTION admission_artifact_link_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE project TEXT; item admission_artifacts%ROWTYPE; held BOOLEAN; count_links BIGINT; published_at BIGINT;
BEGIN
 SELECT holds_task INTO held FROM execution_attempts WHERE attempt_id=NEW.attempt_id AND kind='profiled_v1' FOR UPDATE;
 IF NOT FOUND OR NOT held THEN RAISE EXCEPTION 'artifact link needs held profiled attempt'; END IF;
 SELECT launch_manifest->'bindings'->>'project_id',
        (graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT
 INTO project,published_at FROM profiled_launches WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO item FROM admission_artifacts WHERE artifact_id=NEW.artifact_id FOR SHARE;
 IF NOT FOUND OR item.owner_key IS DISTINCT FROM project THEN RAISE EXCEPTION 'artifact project mismatch'; END IF;
 IF item.offer_staged THEN
  PERFORM 1 FROM admission_offer_stages s JOIN admission_offer_stage_pins p USING(stage_id)
  WHERE s.attempt_id=NEW.attempt_id AND s.project_id::TEXT=project
   AND s.closed_at_ms IS NULL AND s.expires_at_ms>published_at AND p.artifact_id=NEW.artifact_id
  FOR SHARE OF s;
  IF NOT FOUND THEN RAISE EXCEPTION 'artifact needs its current offer stage pin'; END IF;
 END IF;
 IF NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts WHERE attempt_id=NEW.attempt_id AND artifact_id=NEW.artifact_id) THEN
  SELECT count(*) INTO count_links FROM profiled_launch_artifacts WHERE attempt_id=NEW.attempt_id;
  IF count_links>=128 THEN RAISE EXCEPTION 'artifact link capacity'; END IF;
 END IF;
 RETURN NEW;
END $$;

UPDATE meta SET value='167' WHERE key='state_format_version';
