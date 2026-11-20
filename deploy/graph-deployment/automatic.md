# Automatic aq-graph development deployment

A separate Host11 namespace runs the `aq.graph-deployment` playbook.
Host10 continues to own routing, delivery and `delivery_merge_evidence`.
Host10 and Host11 pin different catalog profiles. Keep them on separate
instances with separate AQ databases and use the read-only view below to
connect the instances.

The development target is `aq-graph-deploy-canary`. Enrollment ties one
Host10 project and repository binding generation on `ElectricJack/aq-graph`
to one Host11 project and disposable instance. Start with a fresh instance
directory and follow the [manual canary setup](README.md) for package and
controller prerequisites.

## Enroll the Host10 source

Use a Host10 build whose `pull_request.merge` writes merge evidence and a
database with migration 0161 applied. On that disposable database, create a
dedicated login with no role memberships and set a fresh development password. Run
[`host10-read-scope.sql`](host10-read-scope.sql) as the database owner, replacing
the example shell variables with the enrolled project ID, aq-git repository
ID, repository binding generation and enrollment timestamp in milliseconds.

```sh
psql -v ON_ERROR_STOP=1 -f deploy/graph-deployment/host10-read-scope.sql \
  -v reader=aq_graph_deployment_reader \
  -v project_id="$delivery_project_id" -v repository_id="$repository_id" \
  -v binding_generation="$repository_generation" -v enrolled_after_ms="$enrolled_after_ms"
```

The view exposes operation ID, PR number, confirmed merge SHA and observation
time for the configured repository on `refs/heads/main` at or after enrollment.
The login needs database connection access and the view grants in the script.
Use that login for this private PostgreSQL service file on the Host11 host.

```ini
[aq_graph_deploy]
host=<disposable Host10 PostgreSQL address>
port=<port>
dbname=<Host10 database>
user=aq_graph_deployment_reader
passfile=/absolute/path/to/disposable-reader.pgpass
```

Use mode `0600` for both files and give the Host11 service UID ownership.
Across hosts, use a private transport and configure libpq certificate
verification for the chosen endpoint. The bridge invokes `psql` in read-only
mode. It needs the PostgreSQL client on the Host11 host and supports an
operator-provided absolute `merge_client` path. Install the view once for
this enrollment and use a dedicated login for it.

## Enable the Host11 schedule

Add `merge_source` to the enrolled project's private plugin binding.

```toml
[projects."<Host11 project UUID>"]
environment = "aq-graph-deploy-canary"
directory = "/absolute/path/to/disposable-instance"
merge_source = "/absolute/path/to/reader-service.conf"
```

Seal the draft in `playbook/` with the Host11 catalog and the installed plugin
extension. When using `aq-playbooks authoring seal` offline, copy the four
authoring files into a private workspace, use the Host11 registry for
`catalog.json`, and put the plugin's `name`, `registry` and `notes` in one
object in the `extensions.json` array. Use the resulting artifact in the
isolated Host11 instance's `[[playbooks.deployments]]` entry.

```toml
[[playbooks.deployments]]
binding = "aq.graph-deployment"
artifact_file = "/absolute/path/to/artifact.json"
grant_id = "<grant UUID>"
expires_at_ms = 0 # Replace with the grant's future expiry in epoch milliseconds.
scope = { workspace_id = "<workspace UUID>", project_id = "<Host11 project UUID>", recipients = [], capabilities = ["ext.graph_deploy"] }
schedules = [{ key = "graph-merge", every_ms = 30000 }]
```

Use `profile = "host_v11"` for the isolated namespace. Set up its normal
Graph access and project binding against a disposable control workspace that
is available at Host11 startup. Provide Python and `psql` in the Host11
runtime. The regular Host11 driver polls at least twice per interval. A schedule with no merge source
configured returns `automatic_deployment_disabled`. The manual entry can
operate with just the environment binding.

## Run the builder and reconciler

Run Host11, the builder and the controller on the development host where
that instance's SQLite database and build files live on a local filesystem.
Run these commands periodically with a full-history checkout of the enrolled aq-graph repository whose local
`refs/remotes/origin/main` is kept current by the normal development fetch.

```sh
python3 deploy/graph-deployment/build-image.py "$instance_directory" "$graph_checkout"
deploy/graph-deployment/plugins/graph_deploy/bin/graph-deploy tick "$instance_directory"
```

Schedule the controller often enough to refresh health within 15 seconds.
The builder can run on its own timer and uses a lock to avoid duplicate
publication while a build is active. It reads the pending merge SHA from
the instance database and builds its Git archive after verifying first-parent
membership. It publishes the immutable archive before publishing its
provenance receipt. The next Host11 occurrence registers that exact build and
follows its observed rollout. A missing build returns `waiting_for_build`
and leaves the current generation in place. A build failure can be retried
by running the builder again after correcting the cause.

The bridge rescans the scoped view so a late commit with an older observation
time can still be seen. Operation IDs and their submitted generations
are retained in SQLite. A repeated scan or Host11 restart uses those records
and leaves the same provider instance in place. The builder's first-parent
position fences a late older revision even when it never had a deployment
generation. Each scan is bounded at 10,000 rows and reports
`merge_source_limit` if the enrollment exceeds that size. Keep this
disposable enrollment's observation window sized for the pilot.

Use the playbook receipts to correlate the Host10 merge operation with the
desired generation, provider ID and observed OCI digest. To pause, disable
the binding's schedule or remove its merge source, and stop the instance
timers if you also need to halt already queued work. Preserve the private
SQLite database and provenance directory together when restarting the
canary. A new source enrollment and production promotion are separate
operations.

## Reproduce the Host10 to Host11 canary

```sh
python3 scripts/test-graph-deployment.py --automatic ../../aq-graph \
  aa2590e5b5dc7e79f6a38b28f77f8c3ee91e9a54 \
  5cd9b73e4f54bf06908cd6a8a3a74cdd7fc42a2b
```

The test uses the real Host10 merge-evidence writer with scripted Git and
forge responses, a dedicated PostgreSQL view reader, the real Host11
schedule and driver, and disposable Graph containers. It builds the newer
revision first so the automatic path must wait for the older recorded SHA
before that rollout can start. It checks project, repository, branch and
enrollment isolation, denies read access to the base table and write access
to the view, and verifies restart and delayed-merge behavior.

### Canary result

The `aq-graph-deploy-canary-7d4e12095db2` run passed on November 19, 2026.
The Host10 source database and the Host11 driver database were both
disposable. Each observed digest also passed Graph's HTTP readiness probe.

| Generation | Host10 merge operation | Merged revision | Observed OCI manifest digest |
| --- | --- | --- | --- |
| 1 | `4bfedee5-eb0a-8d02-ab5a-efc7168c12fe` | `aa2590e5b5dc7e79f6a38b28f77f8c3ee91e9a54` | `sha256:42a22d884b5d70688fbb23089a9ed98de8138f82f3196c1e8851818192d07a64` |
| 2 | `bcc2c5f3-7d19-83e7-8e0d-06df19c5c16d` | `5cd9b73e4f54bf06908cd6a8a3a74cdd7fc42a2b` | `sha256:cdbde4023ea003c852cec48ac671d2e5dedf3ac357b843d42c5ad48d5ec4d6a6` |
