-- A terminal reservation pays for the remaining terminal writes of an issued
-- phase grant. A stopped attempt can keep its task while delivery decides what
-- to do. Its terminal proof closes the reservation independently of that claim.
-- Probe terminal receipts by attempt to keep the poll independent of settled history.
SET LOCAL lock_timeout = '5s';

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

UPDATE meta SET value='165' WHERE key='state_format_version';
