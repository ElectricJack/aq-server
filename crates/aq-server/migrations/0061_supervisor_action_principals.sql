-- Retain one operation journal with explicit, disjoint principal domains.
ALTER TABLE dashboard_operations ADD COLUMN owner_id UUID REFERENCES users(id);
ALTER TABLE dashboard_operations ADD COLUMN supervisor_session_id UUID REFERENCES central_session_generations(session_id);
-- Migration holds the table lock in the production migration transaction.
-- Backfill only the new reference, including immutable retired tombstones, then
-- restore the original guard before any application transaction can proceed.
ALTER TABLE dashboard_operations DISABLE TRIGGER dashboard_operation_guard;
UPDATE dashboard_operations SET owner_id=principal_id;
ALTER TABLE dashboard_operations ENABLE TRIGGER dashboard_operation_guard;
ALTER TABLE dashboard_operations DROP CONSTRAINT dashboard_operations_principal_id_fkey;
ALTER TABLE dashboard_operations ADD CONSTRAINT dashboard_operation_principal CHECK (
 (owner_id IS NOT NULL AND owner_id=principal_id AND supervisor_session_id IS NULL)
 OR (owner_id IS NULL AND supervisor_session_id IS NOT NULL AND supervisor_session_id=principal_id
     AND kind IN ('task.create','task.graph.create'))
);
CREATE FUNCTION bind_dashboard_principal() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' THEN
  IF ROW(NEW.owner_id,NEW.supervisor_session_id) IS DISTINCT FROM ROW(OLD.owner_id,OLD.supervisor_session_id) THEN
   RAISE EXCEPTION 'operation principal is immutable';
  END IF;
 ELSIF EXISTS(SELECT 1 FROM users WHERE id=NEW.principal_id) THEN
  NEW.owner_id=NEW.principal_id;
  NEW.supervisor_session_id=NULL;
 ELSE
  NEW.owner_id=NULL;
  NEW.supervisor_session_id=NEW.principal_id;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER dashboard_principal_binding BEFORE INSERT OR UPDATE ON dashboard_operations
 FOR EACH ROW EXECUTE FUNCTION bind_dashboard_principal();
UPDATE meta SET value='61' WHERE key='state_format_version';
