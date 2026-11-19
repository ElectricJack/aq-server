-- Revoking a node releases the attempts it still holds. A revoked node never
-- sends the lost-attempt release (0102), so an attempt granted to it kept its
-- task claim forever. The same release is now recorded for the revocation:
-- the lost-attempt release record (proof kind and reason `node_revoked`, not
-- charged to the lineage budgets), the hot stop that names it, and the B4
-- replacement release that frees the claim (0052), so the task is retryable.
ALTER TABLE lost_attempt_releases DROP CONSTRAINT lost_attempt_releases_proof_kind_check;
ALTER TABLE lost_attempt_releases DROP CONSTRAINT lost_attempt_releases_reason_check;
ALTER TABLE lost_attempt_releases ADD CONSTRAINT lost_attempt_releases_proof_kind_check
  CHECK (proof_kind IN ('boot_changed','cgroup_teardown','node_revoked'));
ALTER TABLE lost_attempt_releases ADD CONSTRAINT lost_attempt_releases_reason_check CHECK (
    (proof_kind = 'boot_changed' AND reason = 'lost_after_reboot')
    OR (proof_kind = 'cgroup_teardown' AND reason = 'lost_after_crash')
    OR (proof_kind = 'node_revoked' AND reason = 'node_revoked'));

-- Releases every attempt of `p_node` that holds its task with a grant and no
-- known stop, as `lost::check_locked` admits it for the node itself: status
-- granted or cancelling, stop running or unknown, no result or stopped report,
-- no recorded agent effect, and a verified source export the retry starts
-- from. Returns the number released. A replay releases nothing: a released
-- attempt no longer holds its task. The caller holds the node's row.
CREATE FUNCTION release_revoked_node_attempts(p_node UUID, p_at TIMESTAMPTZ) RETURNS INTEGER
LANGUAGE plpgsql AS $$
DECLARE r RECORD; rec JSONB; dig TEXT; stop JSONB; keep JSONB; fx JSONB; n INTEGER := 0;
BEGIN
 FOR r IN
  SELECT a.attempt_id, a.task_id, x.operation_id,
         b.manifest->'manifest_digest' AS manifest_digest,
         b.manifest->'bundle_digest' AS bundle_digest,
         b.manifest->'checkpoint' AS checkpoint
  FROM execution_attempts a
  JOIN profiled_authority_hot h USING (attempt_id)
  JOIN profiled_launch_source_exports x USING (attempt_id)
  JOIN source_bases b ON b.attempt_id = x.attempt_id AND b.profiled_export_operation_id = x.operation_id
  WHERE a.node_id = p_node AND a.kind = 'profiled_v1' AND a.holds_task
    AND jsonb_typeof(h.state->'grant') = 'object'
    AND h.state->>'status' IN ('granted','cancelling')
    AND h.state#>>'{stop,kind}' IN ('running','unknown')
    AND NOT EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id = a.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_result_reports p WHERE p.attempt_id = a.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_stopped_receipts s WHERE s.attempt_id = a.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM agent_graph_operations g WHERE g.attempt_id = a.attempt_id)
  ORDER BY a.attempt_id
  FOR UPDATE OF a, h
 LOOP
  rec := jsonb_build_object('attempt_id', r.attempt_id, 'node_id', p_node, 'proof', 'node_revoked',
    'reason', 'node_revoked', 'charged', FALSE);
  dig := encode(sha256(convert_to(rec::TEXT, 'UTF8')), 'hex');
  stop := jsonb_build_object('id', 'aq.lost-attempt-release/' || r.attempt_id::TEXT, 'version', '1', 'digest', dig);
  keep := jsonb_build_object('id', 'aq.source-export/' || r.operation_id::TEXT, 'version', '1',
    'digest', encode(sha256(convert_to(jsonb_build_object('operation_id', r.operation_id,
      'manifest_digest', r.manifest_digest, 'bundle_digest', r.bundle_digest,
      'checkpoint', r.checkpoint)::TEXT, 'UTF8')), 'hex'));
  fx := jsonb_build_object('id', 'aq.lost-attempt-effects/' || r.attempt_id::TEXT, 'version', '1',
    'digest', encode(sha256(convert_to(jsonb_build_object('attempt_id', r.attempt_id, 'graph_operations', 0,
      'mailbox_operations', 0, 'release', stop)::TEXT, 'UTF8')), 'hex'));
  INSERT INTO lost_attempt_releases(attempt_id, node_id, proof_kind, proof, reason, charged, receipt_digest, recorded_at)
  VALUES (r.attempt_id, p_node, 'node_revoked', jsonb_build_object('kind', 'node_revoked'), 'node_revoked', FALSE, dig, p_at);
  UPDATE profiled_authority_hot SET state = state || jsonb_build_object(
    'status', 'finished',
    'stop', jsonb_build_object('kind', 'stopped', 'receipt', stop),
    'retention', jsonb_build_object('kind', 'retained', 'receipt', keep),
    'effects', jsonb_build_object('kind', 'quiescent', 'receipt', fx))
  WHERE attempt_id = r.attempt_id;
  INSERT INTO profiled_replacement_releases(attempt_id, task_id, reason, stop_receipt_digest,
    retained_source_receipt_digest, effect_reconciliation_receipt_digest, attempts_started, recorded_at_ms)
  VALUES (r.attempt_id, r.task_id, 'failed_attempt_replacement', stop->>'digest', keep->>'digest', fx->>'digest',
    (SELECT count(*) FROM execution_attempts t WHERE t.task_id = r.task_id
      AND NOT (NOT t.holds_task AND profiled_never_granted_valid(t.attempt_id))),
    GREATEST(1, (extract(epoch FROM p_at) * 1000)::BIGINT));
  UPDATE execution_attempts SET holds_task = FALSE WHERE attempt_id = r.attempt_id;
  n := n + 1;
 END LOOP;
 RETURN n;
END $$;

-- Attempts already held by revoked nodes.
SELECT release_revoked_node_attempts(id, now()) FROM nodes WHERE revoked_at IS NOT NULL ORDER BY id;
UPDATE meta SET value='154' WHERE key='state_format_version';
