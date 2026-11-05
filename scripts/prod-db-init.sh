#!/bin/sh
# Bootstrap only the named owner's role/database. Refuse retained owner drift.
set -eu
case "$1:$2" in
    aq_graph:graph_database_password|aq_server:server_database_password) ;;
    *) echo 'unsupported database owner' >&2; exit 1 ;;
esac
export PGPASSWORD="$(cat /run/secrets/postgres_password)"
export AQ_DB_OWNER=$1 AQ_DB_PASSWORD_FILE=/run/secrets/$2
psql -h postgres -U agent_q -d agent_q -v ON_ERROR_STOP=1 <<'SQL'
\getenv owner AQ_DB_OWNER
\set password `cat "$AQ_DB_PASSWORD_FILE"`
SELECT format('CREATE ROLE %I LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD %L', :'owner', :'password')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = :'owner') \gexec
SELECT format('CREATE DATABASE %I OWNER %I', :'owner', :'owner')
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = :'owner') \gexec
SELECT d.datdba = r.oid AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AS owner_ok
FROM pg_database d JOIN pg_roles r ON r.rolname = :'owner' WHERE d.datname = :'owner' \gset
\if :owner_ok
SELECT format('REVOKE ALL ON DATABASE %I FROM PUBLIC', :'owner') \gexec
\else
DO $$ BEGIN RAISE EXCEPTION 'existing database owner privileges differ; preserve it and migrate explicitly'; END $$;
\endif
SQL
# Verify retained credentials without overwriting or exposing them.
export PGPASSWORD="$(cat "$AQ_DB_PASSWORD_FILE")"
psql -h postgres -U "$AQ_DB_OWNER" -d "$AQ_DB_OWNER" -v ON_ERROR_STOP=1 -tAc 'SELECT 1' >/dev/null
