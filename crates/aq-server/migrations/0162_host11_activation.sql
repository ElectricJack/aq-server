-- Host catalog 2.4.0 has a new immutable profile. Older namespaces keep their existing pins.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8,9,10,11));
