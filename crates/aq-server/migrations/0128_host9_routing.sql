-- Playbook authoring B2 and B5 (spec 0013 §6, §7).
-- Catalog profile 9 (AQ host catalog 2.2.0): profile 8 plus work.read,
-- work.route, work.unroutable and judgment.assess. Existing profiles,
-- grants, catalogs and rows are unchanged; this only admits new rows.
-- route_needed_emissions records why a task is not asked about again: the
-- route was refused `not_offered` or the playbook said `work.unroutable`
-- (with its reasons), under which active routing artifact and which
-- Settings offers. A new task version, artifact or (for not_offered)
-- offers asks again.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8,9));
ALTER TABLE route_needed_emissions
 ADD COLUMN outcome TEXT CHECK(outcome IN ('not_offered','unroutable')),
 ADD COLUMN reasons JSONB CHECK(reasons IS NULL OR jsonb_typeof(reasons) = 'array'),
 ADD COLUMN artifact_digest TEXT,
 ADD COLUMN options_digest TEXT;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='128' WHERE key='state_format_version';
