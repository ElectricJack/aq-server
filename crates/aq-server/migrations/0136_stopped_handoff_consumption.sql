-- A supervisor turn whose input was handed off and whose generation then
-- stopped (an operator stop, or a host stop before the agent replied) is
-- terminal. Before 0136 only a reply or a not-handed-off resolution counted, so
-- such a reservation could be neither consumed nor cancelled and the
-- conversation waited forever ("An earlier reservation needs inspection").
-- The handed-off input is not sent again; an unknown delivery still holds mail.
CREATE OR REPLACE FUNCTION central_supervisor_turn_complete(p_reservation UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM central_supervisor_dispatches d WHERE d.reservation_id=p_reservation AND (
  EXISTS(SELECT 1 FROM central_supervisor_replies r WHERE r.dispatch_id=d.dispatch_id)
  OR EXISTS(SELECT 1 FROM central_session_generations g WHERE g.actor_id=d.actor_id AND g.generation=d.generation
   AND g.session_id=d.session_id AND g.state_bytes::jsonb#>>'{data,plan,dispatch_id}'=d.dispatch_id::text
   AND g.state_bytes::jsonb#>>'{data,plan,host_operation_id}'=d.host_operation_id::text
   AND g.state_bytes::jsonb#>>'{data,dispatch,kind}'='resolved'
   AND (g.state_bytes::jsonb#>>'{data,dispatch,evidence,disposition}'='not_handed_off'
    OR (g.state_bytes::jsonb#>>'{data,dispatch,evidence,disposition}'='handed_off'
     AND g.state_bytes::jsonb#>>'{data,phase}'='stopped_reported')))))
$$;
-- Release turns already stranded this way without changing original receipts or holds.
UPDATE central_supervisor_reservations SET state='consumed',revision=2
 WHERE state='awaiting_prepared_plan' AND central_supervisor_turn_complete(reservation_id);
UPDATE central_supervisor_actors a SET current_reservation=NULL WHERE EXISTS(
 SELECT 1 FROM central_supervisor_reservations r WHERE r.actor_id=a.actor_id
 AND r.reservation_id=a.current_reservation AND r.state='consumed');
UPDATE meta SET value='136' WHERE key='state_format_version';
