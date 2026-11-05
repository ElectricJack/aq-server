-- Revoke in the same lifecycle transaction, including stop requests, expiry,
-- uncertain stops and HostLost. The generation guard already locks the actor.
CREATE FUNCTION revoke_inactive_supervisor_credentials() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF (NEW.state_bytes::jsonb#>>'{data,phase}') IS DISTINCT FROM 'start_committed'
  AND (NEW.state_bytes::jsonb#>>'{data,phase}') IS DISTINCT FROM 'running_reported' THEN
  UPDATE central_supervisor_credentials
   SET revoked_at_ms=GREATEST(1,(extract(epoch FROM clock_timestamp())*1000)::bigint)
   WHERE actor_id=NEW.actor_id AND generation=NEW.generation AND revoked_at_ms IS NULL;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER central_supervisor_credentials_lifecycle_revoke AFTER UPDATE ON central_session_generations
 FOR EACH ROW EXECUTE FUNCTION revoke_inactive_supervisor_credentials();

-- Reconcile credentials retained by older servers after a stop or successor.
UPDATE central_supervisor_credentials c
 SET revoked_at_ms=GREATEST(1,(extract(epoch FROM clock_timestamp())*1000)::bigint)
 FROM central_session_heads h,central_session_generations g
 WHERE c.revoked_at_ms IS NULL AND c.actor_id=h.actor_id
  AND c.actor_id=g.actor_id AND c.generation=g.generation
  AND (c.generation<>h.current_generation
   OR (g.state_bytes::jsonb#>>'{data,phase}') NOT IN ('start_committed','running_reported'));
UPDATE meta SET value='51' WHERE key='state_format_version';
