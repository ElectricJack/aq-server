-- Spec 0008 phase A: at most 30 offers per node, 240 per project and 960 in all in
-- any rolling hour, measured at each offer's source observation. The poll gate
-- holds visibly first; this fence only catches a race between two polls.
CREATE FUNCTION profiled_launch_offer_rate() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE offered BIGINT; project UUID; n RECORD; p RECORD;
BEGIN
 SELECT o.observed_at_ms,o.project_id INTO offered,project FROM admission_source_observations o WHERE o.observation_id=NEW.source_observation_id;
 IF NOT FOUND THEN RETURN NEW; END IF;
 PERFORM pg_advisory_xact_lock(hashtextextended('aq-sealed-offer-rate-v1',0));
 SELECT * INTO n FROM sealed_history_usage('offers',(NEW.launch_binding#>>'{session,node_id}')::UUID,offered);
 SELECT * INTO p FROM sealed_history_usage('project_offers',project,offered);
 IF n.scope_rows>=30 OR n.total_rows>=960 OR p.scope_rows>=240 THEN
  RAISE EXCEPTION 'sealed history offer rate' USING ERRCODE='54000';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_launch_offer_rate BEFORE INSERT ON profiled_launches FOR EACH ROW EXECUTE FUNCTION profiled_launch_offer_rate();
UPDATE meta SET value='105' WHERE key='state_format_version';
