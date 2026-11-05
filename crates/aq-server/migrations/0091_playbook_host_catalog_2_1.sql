-- Catalog profile 8 (AQ host catalog 2.1.0): candidate.prepare outputs the
-- prepared source OID. Same commands and tables as profile 7; existing
-- profiles, grants, catalogs and rows are unchanged; this only admits new rows.
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8));
UPDATE meta SET value='91' WHERE key='state_format_version';
