-- C5a2: a private material lease is bound to the delegated permit issued for the
-- attempt it was admitted for. Permit-authenticated delivery requires exactly
-- that permit; node-authenticated delivery keeps its existing fences.
ALTER TABLE admission_material_leases ADD COLUMN permit_id UUID REFERENCES delegated_permits(permit_id);
CREATE INDEX admission_material_leases_permit ON admission_material_leases(permit_id) WHERE permit_id IS NOT NULL;
CREATE FUNCTION admission_material_permit_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND OLD.permit_id IS NOT NULL AND NEW.permit_id IS DISTINCT FROM OLD.permit_id
 THEN RAISE EXCEPTION 'material lease permit binding is immutable' USING ERRCODE='23514'; END IF;
 IF NEW.permit_id IS NOT NULL THEN
  -- The bound permit's attempt and node stay those the lease was admitted for.
  PERFORM 1 FROM delegated_permits p WHERE p.permit_id=NEW.permit_id
   AND p.attempt_id=NEW.admitted_attempt_id AND p.node_id=NEW.node_id;
  IF NOT FOUND THEN RAISE EXCEPTION 'material lease permit binding mismatch' USING ERRCODE='23514'; END IF;
  -- Binding happens once, at issuance, and never to revoked authority.
  IF (TG_OP='INSERT' OR OLD.permit_id IS NULL)
     AND EXISTS(SELECT 1 FROM delegated_permits p WHERE p.permit_id=NEW.permit_id AND p.revoked)
  THEN RAISE EXCEPTION 'material lease permit unavailable' USING ERRCODE='23514'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER admission_material_permit_guard BEFORE INSERT OR UPDATE ON admission_material_leases
 FOR EACH ROW EXECUTE FUNCTION admission_material_permit_guard();
UPDATE meta SET value='78' WHERE key='state_format_version';
