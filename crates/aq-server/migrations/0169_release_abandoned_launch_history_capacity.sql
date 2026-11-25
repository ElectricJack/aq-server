-- An owner can settle an abandoned launch with a verified pre-start closure.
-- The hot state preserves the reason it was revoked and can remain cancelling.
-- Once the claim is released, the exact closure and disposition make this
-- retained history inactive for accounting.
SET LOCAL lock_timeout='5s';

CREATE FUNCTION sealed_abandoned_launch_closed(p_attempt UUID) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM profiled_launch_closures c JOIN profiled_stopped_dispositions d USING(attempt_id)
  WHERE c.attempt_id=p_attempt AND d.receipt->>'disposition'='abandoned'
    AND d.receipt->>'stop_receipt_digest'=c.receipt_digest)
$$;

CREATE OR REPLACE FUNCTION sealed_attempt_live(p_attempt UUID) RETURNS BOOLEAN LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h ON h.attempt_id=e.attempt_id
  WHERE e.attempt_id=p_attempt
  AND (e.holds_task OR CASE WHEN e.kind='profiled_v1'
       THEN COALESCE(h.state->>'status','never_granted') NOT IN ('finished','declined','never_granted')
        AND NOT sealed_abandoned_launch_closed(e.attempt_id)
       ELSE COALESCE(e.state->>'status','unknown') NOT IN ('finished','declined','never_granted') END))
$$;

CREATE OR REPLACE VIEW sealed_live_attempt_ids AS
 SELECT e.attempt_id FROM execution_attempts e WHERE e.holds_task
 UNION
 SELECT h.attempt_id FROM profiled_authority_hot h
  WHERE h.state->>'status' NOT IN ('finished','declined','never_granted')
   AND NOT sealed_abandoned_launch_closed(h.attempt_id)
 UNION
 SELECT e.attempt_id FROM execution_attempts e
  WHERE NOT e.holds_task AND e.kind='legacy_fixture'
  AND COALESCE(e.state->>'status','unknown') NOT IN ('finished','declined','never_granted');

SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='169' WHERE key='state_format_version';
