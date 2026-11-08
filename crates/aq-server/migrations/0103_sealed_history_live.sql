-- Spec 0008 phase A: a sealed history row counts toward a cap only while it is
-- live: its run is live, or nothing references it yet and it was written within
-- 10 minutes (pending). One definition for the triggers, the Rust mirror checks,
-- the poll gate and the swarm read. Nothing is deleted.
CREATE FUNCTION sealed_attempt_live(p_attempt UUID) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h ON h.attempt_id=e.attempt_id
  WHERE e.attempt_id=p_attempt
  AND (e.holds_task OR COALESCE(h.state->>'status',e.state->>'status','unknown') NOT IN ('finished','declined','never_granted')))
$$;
CREATE FUNCTION sealed_history_limits()
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
  ('offers','global',960,NULL,1,0),
  ('offers','node',30,NULL,1,0),
  ('phase_terminal','global',256,268435456,1,1048576),
  ('phase_terminal','node',16,16777216,1,1048576),
  ('presence_operations','global',65536,268435456,1,0),
  ('presence_operations','node',4096,16777216,1,0),
  ('presence_rate','global',11520,NULL,1,0),
  ('presence_rate','node',720,NULL,1,0),
  ('project_offers','project',240,NULL,1,0),
  ('source_exports','global',4096,268435456,1,4202496),
  ('source_exports','project',256,67108864,1,4202496),
  ('v2_proposals','global',4096,NULL,1,0),
  ('v2_proposals','node',256,NULL,1,0)
 ) v(k,s,r,b,hr,hb)
$$;
CREATE FUNCTION sealed_history_usage(p_kind TEXT, p_scope UUID, p_now_ms BIGINT, p_exclude UUID DEFAULT NULL)
RETURNS TABLE(total_rows BIGINT, total_bytes BIGINT, scope_rows BIGINT, scope_bytes BIGINT)
LANGUAGE plpgsql STABLE AS $$
BEGIN
 IF p_kind='admission_artifacts' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(octet_length(a.body)),0)::BIGINT,
   (count(*) FILTER(WHERE a.owner_key=p_scope::TEXT))::BIGINT,
   COALESCE(sum(octet_length(a.body)) FILTER(WHERE a.owner_key=p_scope::TEXT),0)::BIGINT
  FROM admission_artifacts a
  WHERE EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id AND sealed_attempt_live(l.attempt_id))
   OR (a.created_at_ms>=p_now_ms-600000 AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id));
 ELSIF p_kind='intelligence_decisions' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(pg_column_size(d.decision)),0)::BIGINT,
   count(*)::BIGINT,COALESCE(sum(pg_column_size(d.decision)),0)::BIGINT
  FROM intelligence_decisions d
  WHERE EXISTS(SELECT 1 FROM profiled_launches l WHERE l.decision_id=d.decision_id AND sealed_attempt_live(l.attempt_id))
   OR (COALESCE((d.decision#>>'{inputs,now_ms}')::BIGINT,0)>=p_now_ms-600000
       AND NOT EXISTS(SELECT 1 FROM profiled_launches l WHERE l.decision_id=d.decision_id));
 ELSIF p_kind IN ('source_exports','staging_exports') THEN
  RETURN QUERY SELECT count(*)::BIGINT,
   COALESCE(sum(CASE WHEN p_kind='staging_exports' THEN e.charged_bytes
     ELSE octet_length(e.request_bytes)+COALESCE(octet_length(e.verified_bytes),4194304) END),0)::BIGINT,
   (count(*) FILTER(WHERE e.project_id=p_scope))::BIGINT,
   COALESCE(sum(CASE WHEN p_kind='staging_exports' THEN e.charged_bytes
     ELSE octet_length(e.request_bytes)+COALESCE(octet_length(e.verified_bytes),4194304) END) FILTER(WHERE e.project_id=p_scope),0)::BIGINT
  FROM profiled_source_exports e
  WHERE e.operation_id IS DISTINCT FROM p_exclude
   AND (e.state='pending'
    OR EXISTS(SELECT 1 FROM profiled_launch_source_exports l WHERE l.operation_id=e.operation_id AND sealed_attempt_live(l.attempt_id))
    OR EXISTS(SELECT 1 FROM source_bases b WHERE b.profiled_export_operation_id=e.operation_id AND sealed_attempt_live(b.attempt_id)));
 ELSIF p_kind='native_preparation' THEN
  RETURN QUERY WITH leases AS (
    SELECT m.lease_id,m.node_id,octet_length(m.lease::text)::BIGINT AS bytes FROM admission_material_leases m
    WHERE m.state='pending' OR sealed_attempt_live(COALESCE(m.admitted_attempt_id,m.prospective_attempt_id))),
   charged AS (
    SELECT l.node_id,l.bytes FROM leases l
    UNION ALL SELECT r.node_id,octet_length(r.response::text)::BIGINT FROM profiled_execution_receipts r
    UNION ALL SELECT l.node_id,octet_length(mr.receipt::text)::BIGINT FROM admission_material_receipts mr JOIN leases l ON l.lease_id=mr.lease_id
    UNION ALL SELECT p.node_id,(octet_length(p.proposal::text)+octet_length(p.policy::text))::BIGINT FROM profiled_v2_proposals p WHERE sealed_attempt_live(p.attempt_id)
    UNION ALL SELECT a.node_id,(octet_length(a.request::text)+octet_length(a.receipt::text))::BIGINT FROM profiled_v2_accepts a WHERE sealed_attempt_live(a.attempt_id)
    UNION ALL SELECT g.node_id,octet_length(g.request_bytes)::BIGINT+octet_length(g.receipt_bytes)+octet_length(g.provenance_bytes)
     FROM profiled_phase_execution_grants g WHERE sealed_attempt_live(g.attempt_id))
  SELECT (SELECT count(*) FROM leases)::BIGINT,(SELECT COALESCE(sum(c.bytes),0) FROM charged c)::BIGINT,
   (SELECT count(*) FROM leases l WHERE l.node_id=p_scope)::BIGINT,(SELECT COALESCE(sum(c.bytes),0) FROM charged c WHERE c.node_id=p_scope)::BIGINT;
 ELSIF p_kind='v2_proposals' THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,(count(*) FILTER(WHERE p.node_id=p_scope))::BIGINT,0::BIGINT
  FROM profiled_v2_proposals p WHERE sealed_attempt_live(p.attempt_id);
 ELSIF p_kind='launch_registrations' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(r.logical_bytes),0)::BIGINT,
   (count(*) FILTER(WHERE r.node_id=p_scope))::BIGINT,COALESCE(sum(r.logical_bytes) FILTER(WHERE r.node_id=p_scope),0)::BIGINT
  FROM profiled_launch_registrations r WHERE sealed_attempt_live(r.attempt_id);
 ELSIF p_kind='phase_terminal' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(c.reserved_bytes),0)::BIGINT,
   (count(*) FILTER(WHERE c.node_id=p_scope))::BIGINT,COALESCE(sum(c.reserved_bytes) FILTER(WHERE c.node_id=p_scope),0)::BIGINT
  FROM profiled_phase_terminal_capacity c WHERE sealed_attempt_live(c.attempt_id);
 ELSIF p_kind='presence_operations' THEN
  RETURN QUERY SELECT count(*)::BIGINT,COALESCE(sum(octet_length(o.request_bytes)+octet_length(o.receipt_bytes)+1024),0)::BIGINT,
   (count(*) FILTER(WHERE o.node_id=p_scope))::BIGINT,
   COALESCE(sum(octet_length(o.request_bytes)+octet_length(o.receipt_bytes)+1024) FILTER(WHERE o.node_id=p_scope),0)::BIGINT
  FROM profiled_presence_operations o
  WHERE o.expires_at_ms>p_now_ms OR (o.kind='presence' AND sealed_attempt_live(o.attempt_id));
 ELSIF p_kind='presence_rate' THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,(count(*) FILTER(WHERE o.node_id=p_scope))::BIGINT,0::BIGINT
  FROM profiled_presence_operations o WHERE o.recorded_at_ms>p_now_ms-3600000;
 ELSIF p_kind IN ('offers','project_offers') THEN
  RETURN QUERY SELECT count(*)::BIGINT,0::BIGINT,
   (count(*) FILTER(WHERE (p_kind='offers' AND l.launch_binding#>>'{session,node_id}'=p_scope::TEXT)
                       OR (p_kind='project_offers' AND o.project_id=p_scope)))::BIGINT,0::BIGINT
  FROM profiled_launches l JOIN admission_source_observations o ON o.observation_id=l.source_observation_id
  WHERE o.observed_at_ms>p_now_ms-3600000;
 ELSE
  RAISE EXCEPTION 'unknown sealed history kind %',p_kind;
 END IF;
END $$;
CREATE FUNCTION sealed_staging_live_bytes() RETURNS BIGINT LANGUAGE sql STABLE AS $$
 SELECT (COALESCE((SELECT sum((t.manifest->>'manifest_bytes')::BIGINT+(t.manifest->>'bundle_bytes')::BIGINT) FROM source_transfers t WHERE NOT t.staging_released),0)
  +COALESCE((SELECT sum(b.reserved_bytes) FROM source_bases b),0)
  +(SELECT u.total_bytes FROM sealed_history_usage('staging_exports',NULL,0) u))::BIGINT
$$;
-- Node call (p_project NULL): node and server limits. Project call: project limits.
CREATE FUNCTION sealed_history_holds(p_node UUID, p_project UUID, p_now_ms BIGINT)
RETURNS TABLE(hold_code TEXT, hold_kind TEXT, hold_scope TEXT, hold_unit TEXT, hold_used BIGINT, hold_limit BIGINT)
LANGUAGE plpgsql STABLE AS $$
DECLARE l RECORD; u RECORD; r BIGINT; b BIGINT;
BEGIN
 FOR l IN SELECT * FROM sealed_history_limits() x ORDER BY x.kind,x.scope LOOP
  IF p_project IS NULL THEN
   CONTINUE WHEN l.scope='project' OR p_node IS NULL;
  ELSE
   CONTINUE WHEN l.scope<>'project';
  END IF;
  SELECT * INTO u FROM sealed_history_usage(l.kind,COALESCE(p_project,p_node),p_now_ms);
  r:=CASE WHEN l.scope='global' THEN u.total_rows ELSE u.scope_rows END;
  b:=CASE WHEN l.scope='global' THEN u.total_bytes ELSE u.scope_bytes END;
  hold_code:=CASE WHEN l.kind IN ('offers','project_offers','presence_rate') THEN 'history_rate_limited' ELSE 'history_cap_reached' END;
  hold_kind:=l.kind; hold_scope:=l.scope;
  IF r+l.headroom_rows>l.max_rows THEN
   hold_unit:='rows'; hold_used:=r; hold_limit:=l.max_rows; RETURN NEXT;
  ELSIF l.max_bytes IS NOT NULL AND b+l.headroom_bytes>l.max_bytes THEN
   hold_unit:='bytes'; hold_used:=b; hold_limit:=l.max_bytes; RETURN NEXT;
  END IF;
 END LOOP;
END $$;
CREATE INDEX sealed_launch_artifact_artifact ON profiled_launch_artifacts(artifact_id);
CREATE INDEX sealed_launch_decision ON profiled_launches(decision_id);
CREATE INDEX sealed_launch_source_observation ON profiled_launches(source_observation_id);
CREATE INDEX sealed_source_observation_time ON admission_source_observations(observed_at_ms);
CREATE INDEX sealed_material_lease_attempt ON admission_material_leases(admitted_attempt_id);
CREATE INDEX sealed_presence_operation_time ON profiled_presence_operations(node_id,recorded_at_ms);
CREATE INDEX sealed_source_base_export ON source_bases(profiled_export_operation_id) WHERE profiled_export_operation_id IS NOT NULL;
UPDATE meta SET value='103' WHERE key='state_format_version';
