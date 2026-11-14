-- Rolling deletion keeps what live work still reads (architecture 0006). The bridge's
-- effect rows and host operations are read back by the engine (reconcile after a lost
-- acknowledgement) and by candidate.publish (evidence) for as long as their run is
-- live. The engine keeps finished runs in aq_playbooks.run_history, so a run, and its
-- root, having a 'runs' row there is the proof that the engine no longer asks. Anything
-- else (no engine schema, no privilege, a run not or no longer in history) answers
-- false: the row is kept.
CREATE FUNCTION playbook_run_finished(p_namespace uuid, p_generation bigint, p_run text)
RETURNS boolean LANGUAGE plpgsql STABLE AS $$
DECLARE done boolean;
BEGIN
 IF to_regclass('aq_playbooks.run_history') IS NULL THEN RETURN false; END IF;
 SELECT EXISTS(SELECT 1 FROM aq_playbooks.run_history c JOIN aq_playbooks.run_history r
   ON r.namespace=c.namespace AND r.kind='runs' AND r.run_id=c.root_id
  WHERE c.namespace='aq/'||p_namespace::text||'/'||p_generation::text AND c.kind='runs' AND c.run_id=p_run)
  INTO done;
 RETURN done;
EXCEPTION WHEN insufficient_privilege THEN RETURN false;
END $$;
UPDATE meta SET value='128' WHERE key='state_format_version';
