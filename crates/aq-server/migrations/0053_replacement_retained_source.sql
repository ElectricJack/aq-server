-- A replacement shares the predecessor's verified export, never another bundle.
ALTER TABLE profiled_launch_source_exports DROP CONSTRAINT profiled_launch_source_exports_operation_id_key;
CREATE INDEX profiled_launch_source_exports_operation ON profiled_launch_source_exports(operation_id);
ALTER TABLE source_bases DROP CONSTRAINT source_bases_operation_id_key;
CREATE UNIQUE INDEX source_bases_legacy_operation ON source_bases(operation_id) WHERE profiled_export_operation_id IS NULL;

-- The existing link and wrapper guards still require exact project/repository/
-- revision/base/manifest bytes. Sharing also requires every previous consumer
-- to be a released failed attempt of this same task with its retention receipt.
CREATE FUNCTION profiled_replacement_source_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE task UUID;
BEGIN
 SELECT task_id INTO STRICT task FROM execution_attempts WHERE attempt_id=NEW.attempt_id;
 IF EXISTS(
  SELECT 1 FROM profiled_launch_source_exports x
  JOIN execution_attempts a USING(attempt_id)
  LEFT JOIN profiled_replacement_releases r USING(attempt_id)
  LEFT JOIN profiled_authority_hot h USING(attempt_id)
  WHERE x.operation_id=NEW.operation_id AND x.attempt_id<>NEW.attempt_id
   AND (a.task_id<>task OR a.holds_task OR r.task_id IS DISTINCT FROM task
        OR r.retained_source_receipt_digest IS DISTINCT FROM h.state#>>'{retention,receipt,digest}')
 ) THEN RAISE EXCEPTION 'shared source requires released predecessor retention receipt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_replacement_source_guard BEFORE INSERT ON profiled_launch_source_exports
 FOR EACH ROW EXECUTE FUNCTION profiled_replacement_source_guard();
UPDATE meta SET value='53' WHERE key='state_format_version';
