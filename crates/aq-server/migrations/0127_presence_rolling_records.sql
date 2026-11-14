-- Architecture 0006: the worker heartbeat log (profiled_presence_operations) is a
-- rolling record. 0029's trigger refused every UPDATE and DELETE, so the log only
-- grew (one row per node about every 41 s). Rows stay immutable: UPDATE is still
-- refused, and so is an ordinary DELETE. Only row retention (aq.row_retention, as
-- 0064 allows for observation history) may delete; it keeps each node's newest
-- operations (aq_row_retention::presence_operations, [workers] heartbeat_log_keep).
-- The trigger profiled_presence_operation_immutable keeps calling this function.
CREATE OR REPLACE FUNCTION refuse_profiled_presence_history_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' AND current_setting('aq.row_retention',true)='on' THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'immutable scoped presence operation';
END $$;
UPDATE meta SET value='127' WHERE key='state_format_version';
