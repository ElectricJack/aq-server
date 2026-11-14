-- Architecture 0006: the native registration history is a rolling record. Its
-- quota trigger refused a node's registration for good at 1,024 rows for the node
-- (one per boot) or 4,096 in all, and nothing could delete a row. The quota is
-- removed. Rows stay immutable: UPDATE and an ordinary DELETE are still refused;
-- only row retention (aq.row_retention) may delete, and it keeps each node's
-- newest rows and every row a launch, a closure or a grant still names
-- (aq_row_retention::registration_history, [workers.registration_history]).
SET LOCAL lock_timeout = '5s';
DROP TRIGGER profiled_registration_history_quota ON profiled_native_registration_history;
DROP FUNCTION profiled_registration_history_quota();
ALTER TABLE profiled_native_registration_history
 ADD COLUMN recorded_at_ms BIGINT NOT NULL DEFAULT (extract(epoch FROM clock_timestamp())*1000)::bigint;
CREATE INDEX profiled_native_registration_history_node_age
 ON profiled_native_registration_history(node_id,recorded_at_ms);
CREATE FUNCTION refuse_profiled_registration_history_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' AND current_setting('aq.row_retention',true)='on' THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'immutable profiled launch closure history';
END $$;
DROP TRIGGER profiled_registration_history_immutable ON profiled_native_registration_history;
CREATE TRIGGER profiled_registration_history_immutable BEFORE DELETE OR UPDATE ON profiled_native_registration_history
 FOR EACH ROW EXECUTE FUNCTION refuse_profiled_registration_history_change();
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='132' WHERE key='state_format_version';
