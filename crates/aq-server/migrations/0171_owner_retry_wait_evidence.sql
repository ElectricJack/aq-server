-- An admitted retry remains actionable while its exact predecessor's evidence settles.
ALTER TABLE owner_task_retry_intents ADD COLUMN wait_reason TEXT
 CHECK(wait_reason IN ('replacement_prior_execution_uncertain','replacement_effects_unreconciled',
                       'replacement_source_retention_unknown','replacement_lineage_history_unknown'));

CREATE OR REPLACE FUNCTION guard_owner_task_retry_intent() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP <> 'UPDATE' OR
    (to_jsonb(NEW)-ARRAY['next_check_at','checks','released_at','wait_reason']) IS DISTINCT FROM
      (to_jsonb(OLD)-ARRAY['next_check_at','checks','released_at','wait_reason']) OR
    (OLD.released_at IS NOT NULL AND NEW.released_at IS DISTINCT FROM OLD.released_at)
 THEN
  RAISE EXCEPTION 'owner task retry intent and release evidence are immutable';
 END IF;
 RETURN NEW;
END $$;

UPDATE meta SET value='171' WHERE key='state_format_version';
