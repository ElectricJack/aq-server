-- A supervisor turn that stopped without a reply holds its mail again (0136
-- consumed it, which lost the messages of every turn whose agent failed, such
-- as an expired subscription login). The owner retries it: after a stop and
-- start the host cancels the reservation, which this guard now allows once the
-- turn's generation has stopped, and the next turn reserves the same mail.
-- Reservations 0136 already consumed stay consumed and remain valid proofs: a
-- handed-off stopped turn counts as complete only for a reservation that is
-- already consumed, so consumption (which runs while it is awaiting) never
-- takes that path again.
CREATE OR REPLACE FUNCTION central_supervisor_turn_complete(p_reservation UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM central_supervisor_dispatches d WHERE d.reservation_id=p_reservation AND (
  EXISTS(SELECT 1 FROM central_supervisor_replies r WHERE r.dispatch_id=d.dispatch_id)
  OR EXISTS(SELECT 1 FROM central_session_generations g WHERE g.actor_id=d.actor_id AND g.generation=d.generation
   AND g.session_id=d.session_id AND g.state_bytes::jsonb#>>'{data,plan,dispatch_id}'=d.dispatch_id::text
   AND g.state_bytes::jsonb#>>'{data,plan,host_operation_id}'=d.host_operation_id::text
   AND g.state_bytes::jsonb#>>'{data,dispatch,kind}'='resolved'
   AND (g.state_bytes::jsonb#>>'{data,dispatch,evidence,disposition}'='not_handed_off'
    OR (g.state_bytes::jsonb#>>'{data,dispatch,evidence,disposition}'='handed_off'
     AND g.state_bytes::jsonb#>>'{data,phase}'='stopped_reported'
     AND EXISTS(SELECT 1 FROM central_supervisor_reservations r
      WHERE r.reservation_id=p_reservation AND r.state='consumed'))))))
$$;
CREATE OR REPLACE FUNCTION guard_central_reservation_state() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'central reservation history cannot be removed'; END IF;
 IF (to_jsonb(NEW)-ARRAY['state','revision','cancel_operation_id','cancel_actor_revision']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision','cancel_operation_id','cancel_actor_revision'])
  OR OLD.state<>'awaiting_prepared_plan' OR NEW.state NOT IN ('cancelled_before_plan','consumed') OR NEW.revision<>OLD.revision+1
  OR OLD.cancel_operation_id IS NOT NULL OR OLD.cancel_actor_revision IS NOT NULL THEN
  RAISE EXCEPTION 'central reservation is immutable except one pre-plan cancellation';
 END IF;
 IF NEW.state='consumed' AND NOT central_supervisor_turn_complete(NEW.reservation_id) THEN
  RAISE EXCEPTION 'central reservation consumption requires terminal turn evidence';
 END IF;
 IF NEW.state='cancelled_before_plan' AND EXISTS(SELECT 1 FROM central_supervisor_dispatches WHERE reservation_id=NEW.reservation_id)
  AND NOT EXISTS(SELECT 1 FROM central_supervisor_dispatches d JOIN central_session_generations g
   ON g.actor_id=d.actor_id AND g.generation=d.generation AND g.session_id=d.session_id
   WHERE d.reservation_id=NEW.reservation_id
   AND g.state_bytes::jsonb#>>'{data,phase}' IN ('stopped_reported','cancelled_before_start')) THEN
  RAISE EXCEPTION 'dispatched central reservation can be released only after its turn stopped';
 END IF;
 RETURN NEW;
END $$;
UPDATE meta SET value='137' WHERE key='state_format_version';
