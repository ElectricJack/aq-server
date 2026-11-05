#!/bin/sh
# The local engine owns its schema in the SAME physical AQ database.
set -eu
PGPASSWORD=$(cat /run/secrets/postgres_password)
export PGPASSWORD
engine_password=$(cat /run/secrets/playbook_database_password)
test "${#engine_password}" = 64
case "$engine_password" in *[!a-f0-9]*) exit 1;; esac
psql -h postgres -U agent_q -d agent_q -v ON_ERROR_STOP=1 <<'SQL'
\set engine_password `cat /run/secrets/playbook_database_password`
SELECT format('CREATE ROLE aq_playbook LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD %L', :'engine_password')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname='aq_playbook') \gexec
-- The frozen engine runs CREATE SCHEMA IF NOT EXISTS on every open; PostgreSQL
-- requires database CREATE even when the dedicated schema already exists.
GRANT CREATE ON DATABASE agent_q TO aq_playbook;
CREATE SCHEMA IF NOT EXISTS aq_playbooks AUTHORIZATION aq_playbook;
SQL
owner=$(psql -h postgres -U agent_q -d agent_q -tAc "SELECT r.rolname || ':' || r.rolsuper::text || ':' || r.rolcreatedb::text || ':' || r.rolcreaterole::text FROM pg_namespace n JOIN pg_roles r ON r.oid=n.nspowner WHERE n.nspname='aq_playbooks'")
test "$owner" = 'aq_playbook:false:false:false' || { echo 'retained playbook schema/role differs; use a fresh Compose project' >&2; exit 1; }
# Verify retained credentials without changing an existing role's password.
export PGPASSWORD="$engine_password"
psql -h postgres -U aq_playbook -d agent_q -v ON_ERROR_STOP=1 -tAc 'SELECT 1' >/dev/null
