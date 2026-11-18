-- This check runs repeatedly while counting occupied slots. Keep its joins in
-- written order so checking several finished attempts fits the dashboard read
-- budget, while continuing to validate the same stop evidence.
ALTER FUNCTION profiled_compute_stopped_valid(UUID, UUID)
  SET join_collapse_limit TO 1;

UPDATE meta SET value='141' WHERE key='state_format_version';
