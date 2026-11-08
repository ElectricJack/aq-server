-- Spec 0008 phase A: a sealed history row counts toward a cap only while it is
-- live: its run is live, or nothing references it yet and it was written within
-- 10 minutes (pending). One definition for the triggers, the Rust mirror checks,
-- the poll gate and the swarm read. Nothing is deleted.
CREATE FUNCTION sealed_attempt_live(p_attempt UUID) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h ON h.attempt_id=e.attempt_id
  WHERE e.attempt_id=p_attempt
  AND (e.holds_task OR CASE WHEN e.kind='profiled_v1'
       THEN COALESCE(h.state->>'status','never_granted')
       ELSE COALESCE(e.state->>'status','unknown') END NOT IN ('finished','declined','never_granted')))
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
-- A source export carries its creation time so a verified export that no run
-- links yet stays live for the pending window (no staging overshoot while a
-- selection is assembled). NULL for rows written before this migration.
ALTER TABLE profiled_source_exports ADD COLUMN created_at_ms BIGINT CHECK(created_at_ms>0);
-- The live attempt set. Every usage query starts here and joins out to the
-- history tables through their attempt keys, so its cost follows the number of
-- live attempts, not the size of the history. An attempt is live while it holds
-- its task; a profiled attempt (the native path) stays live while its hot
-- status is not settled; a legacy-fixture attempt, which has no hot row, while
-- its snapshot status is not settled. A released profiled attempt without a hot
-- row is settled: it holds nothing and no owner will advance it. Same
-- definition as sealed_attempt_live().
CREATE VIEW sealed_live_attempt_ids AS
 SELECT e.attempt_id FROM execution_attempts e WHERE e.holds_task
 UNION
 SELECT h.attempt_id FROM profiled_authority_hot h
  WHERE (h.state->>'status') NOT IN ('finished','declined','never_granted')
 UNION
 SELECT e.attempt_id FROM execution_attempts e
  WHERE NOT e.holds_task AND e.kind='legacy_fixture'
  AND COALESCE(e.state->>'status','unknown') NOT IN ('finished','declined','never_granted');
-- Live usage of one kind, grouped by its scope (node, project, or NULL for a
-- kind with none). Totals are the sum over all groups. p_now_ms of 0 turns the
-- pending windows off for the kinds that have one.
CREATE FUNCTION sealed_history_scoped(p_kind TEXT, p_now_ms BIGINT, p_exclude UUID DEFAULT NULL, p_live UUID[] DEFAULT NULL)
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
 ELSIF p_kind='presence_rate' THEN
  RETURN QUERY SELECT o.node_id,count(*)::BIGINT,0::BIGINT
   FROM profiled_presence_operations o WHERE o.recorded_at_ms>p_now_ms-3600000 GROUP BY o.node_id;
 ELSIF p_kind='offers' THEN
  RETURN QUERY SELECT CASE WHEN l.launch_binding#>>'{session,node_id}' ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    THEN (l.launch_binding#>>'{session,node_id}')::UUID END,count(*)::BIGINT,0::BIGINT
   FROM admission_source_observations o JOIN profiled_launches l ON l.source_observation_id=o.observation_id
   WHERE o.observed_at_ms>p_now_ms-3600000 GROUP BY 1;
 ELSIF p_kind='project_offers' THEN
  RETURN QUERY SELECT o.project_id,count(*)::BIGINT,0::BIGINT
   FROM admission_source_observations o JOIN profiled_launches l ON l.source_observation_id=o.observation_id
   WHERE o.observed_at_ms>p_now_ms-3600000 GROUP BY o.project_id;
 ELSE
  RAISE EXCEPTION 'unknown sealed history kind %',p_kind;
 END IF;
END $$;
CREATE FUNCTION sealed_history_usage(p_kind TEXT, p_scope UUID, p_now_ms BIGINT, p_exclude UUID DEFAULT NULL)
RETURNS TABLE(total_rows BIGINT, total_bytes BIGINT, scope_rows BIGINT, scope_bytes BIGINT)
LANGUAGE sql STABLE AS $$
 SELECT COALESCE(sum(s.s_rows),0)::BIGINT,COALESCE(sum(s.s_bytes),0)::BIGINT,
  COALESCE(sum(s.s_rows) FILTER(WHERE s.s_scope IS NOT DISTINCT FROM p_scope),0)::BIGINT,
  COALESCE(sum(s.s_bytes) FILTER(WHERE s.s_scope IS NOT DISTINCT FROM p_scope),0)::BIGINT
 FROM sealed_history_scoped(p_kind,p_now_ms,p_exclude) s
$$;
CREATE FUNCTION sealed_staging_live_bytes(p_now_ms BIGINT DEFAULT 0) RETURNS BIGINT LANGUAGE sql STABLE AS $$
 SELECT (COALESCE((SELECT sum((t.manifest->>'manifest_bytes')::BIGINT+(t.manifest->>'bundle_bytes')::BIGINT) FROM source_transfers t WHERE NOT t.staging_released),0)
  +COALESCE((SELECT sum(b.reserved_bytes) FROM source_bases b),0)
  +(SELECT u.total_bytes FROM sealed_history_usage('staging_exports',NULL,p_now_ms) u))::BIGINT
$$;
-- Holds for a poll or the swarm: node and server limits for every listed node
-- (server limits once), project limits for every listed project. Each kind's
-- live usage is computed once per call, whatever the number of nodes.
CREATE FUNCTION sealed_history_holds(p_nodes UUID[], p_projects UUID[], p_now_ms BIGINT)
RETURNS TABLE(hold_code TEXT, hold_kind TEXT, hold_scope TEXT, hold_scope_id UUID, hold_unit TEXT, hold_used BIGINT, hold_limit BIGINT)
LANGUAGE plpgsql STABLE AS $$
DECLARE k TEXT; l RECORD; live UUID[] := COALESCE((SELECT array_agg(v.attempt_id) FROM sealed_live_attempt_ids v),'{}'); ids UUID[]; rs BIGINT[]; bs BIGINT[]; tr BIGINT; tb BIGINT; scopes UUID[]; sid UUID; i INT; r BIGINT; b BIGINT;
BEGIN
 FOR k IN SELECT DISTINCT x.kind FROM sealed_history_limits() x ORDER BY 1 LOOP
  CONTINUE WHEN NOT EXISTS(SELECT 1 FROM sealed_history_limits() x WHERE x.kind=k
   AND ((x.scope='project' AND cardinality(p_projects)>0) OR (x.scope<>'project' AND cardinality(p_nodes)>0)));
  SELECT COALESCE(sum(s.s_rows),0),COALESCE(sum(s.s_bytes),0),
   COALESCE(array_agg(s.s_scope) FILTER(WHERE s.s_scope IS NOT NULL),'{}'),
   COALESCE(array_agg(s.s_rows) FILTER(WHERE s.s_scope IS NOT NULL),'{}'),
   COALESCE(array_agg(s.s_bytes) FILTER(WHERE s.s_scope IS NOT NULL),'{}')
  INTO tr,tb,ids,rs,bs FROM sealed_history_scoped(k,p_now_ms,NULL,live) s;
  FOR l IN SELECT * FROM sealed_history_limits() x WHERE x.kind=k ORDER BY x.scope LOOP
   IF l.scope='global' THEN
    IF cardinality(p_nodes)=0 THEN CONTINUE; END IF;
    scopes:=ARRAY[NULL::UUID];
   ELSIF l.scope='node' THEN scopes:=p_nodes;
   ELSE scopes:=p_projects;
   END IF;
   IF scopes IS NULL OR cardinality(scopes)=0 THEN CONTINUE; END IF;
   FOREACH sid IN ARRAY scopes LOOP
    IF l.scope='global' THEN r:=tr; b:=tb;
    ELSE
     i:=array_position(ids,sid);
     r:=COALESCE(rs[i],0); b:=COALESCE(bs[i],0);
    END IF;
    hold_code:=CASE WHEN k IN ('offers','project_offers','presence_rate') THEN 'history_rate_limited' ELSE 'history_cap_reached' END;
    hold_kind:=k; hold_scope:=l.scope; hold_scope_id:=sid;
    IF r+l.headroom_rows>l.max_rows THEN
     hold_unit:='rows'; hold_used:=r; hold_limit:=l.max_rows; RETURN NEXT;
    ELSIF l.max_bytes IS NOT NULL AND b+l.headroom_bytes>l.max_bytes THEN
     hold_unit:='bytes'; hold_used:=b; hold_limit:=l.max_bytes; RETURN NEXT;
    END IF;
   END LOOP;
  END LOOP;
 END LOOP;
END $$;
CREATE INDEX sealed_launch_artifact_artifact ON profiled_launch_artifacts(artifact_id);
CREATE INDEX sealed_launch_decision ON profiled_launches(decision_id);
CREATE INDEX sealed_launch_source_observation ON profiled_launches(source_observation_id);
CREATE INDEX sealed_source_observation_time ON admission_source_observations(observed_at_ms);
CREATE INDEX sealed_material_lease_prospective ON admission_material_leases(prospective_attempt_id);
CREATE INDEX sealed_material_lease_pending ON admission_material_leases(lease_id) WHERE state='pending';
CREATE INDEX sealed_presence_operation_time ON profiled_presence_operations(node_id,recorded_at_ms);
CREATE INDEX sealed_presence_recorded ON profiled_presence_operations(recorded_at_ms);
CREATE INDEX sealed_presence_expiry ON profiled_presence_operations(expires_at_ms);
CREATE INDEX sealed_presence_attempt ON profiled_presence_operations(attempt_id) WHERE attempt_id IS NOT NULL;
CREATE INDEX sealed_source_base_export ON source_bases(profiled_export_operation_id) WHERE profiled_export_operation_id IS NOT NULL;
CREATE INDEX sealed_artifact_created ON admission_artifacts(created_at_ms);
CREATE INDEX sealed_decision_time ON intelligence_decisions(((decision#>>'{inputs,now_ms}')::BIGINT));
CREATE INDEX sealed_export_pending ON profiled_source_exports(operation_id) WHERE state='pending';
CREATE INDEX sealed_export_created ON profiled_source_exports(created_at_ms) WHERE created_at_ms IS NOT NULL;
CREATE INDEX sealed_attempt_held ON execution_attempts(attempt_id) WHERE holds_task;
CREATE INDEX sealed_hot_unsettled ON profiled_authority_hot(attempt_id)
 WHERE (state->>'status') NOT IN ('finished','declined','never_granted');
CREATE INDEX sealed_attempt_unsettled ON execution_attempts(attempt_id)
 WHERE NOT holds_task AND kind='legacy_fixture'
 AND COALESCE(state->>'status','unknown') NOT IN ('finished','declined','never_granted');
UPDATE meta SET value='103' WHERE key='state_format_version';
