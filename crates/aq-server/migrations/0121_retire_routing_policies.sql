-- Retire the routing decision table (spec 0013 §8): a route is decided by an
-- authored routing playbook, not by a compiled policy, so the compiles and
-- the active-policy pointer of 0120 go. The switch needs an active routing
-- playbook and none is active yet, so a switch left on is turned off; task
-- routes and route-needed emissions are kept.
UPDATE routing_state SET enabled = FALSE;
DROP TABLE routing_policies;
ALTER TABLE routing_state DROP COLUMN active_policy_digest;
UPDATE meta SET value='121' WHERE key='state_format_version';
