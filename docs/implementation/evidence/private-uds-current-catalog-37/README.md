# Actual private UDS current-catalog owner on AQ 37

Passed on 2026-11-01 with the composed default-feature backend from
`5d445a50644b32d5318f974b9c41e4700769fc17` and exact immutable dashboard bundle
`624707540cc6566158c3721e4733f69934c8a1670c4ffb972afb7d469fb8e663`.
This extends the historical state35/owner0.1.2 inert proof to runner/PostgreSQL
0.1.3 with current catalog inspection. It uses real PostgreSQL Unix sockets;
there is no synthetic forwarding, browser interception or test-support backend.
Canonical AQ remains outside this proof. The isolated AQ listeners are stopped.

The same retained private PostgreSQL instance, database, directory and real
socket passed independent transport inspection before startup and after final
shutdown. Physical cluster ID is `7703296609124593685`; actual peer UID/GID1000,
socket0600, directory0700, peer authentication, no TCP listener and no published
ports are retained. AQ itself does not perform SO_PEERCRED or physical system-ID
comparison: those are independent operator observations in this proof.

## Preserved authority and honest metadata

The retained private database advanced AQ35→37 and component schema3→4 in place.
A consistent pre-upgrade private custom-format pg_dump remains outside git,
0600; only its pin is recorded. No rows were removed or reseeded to manufacture
empty activity. The artifact, config, engine and bridge key files, core URL,
engine connection file, namespace, grant and binding identities are unchanged.
File hashes and grant payload digest are retained in `before.json` and checked
after both successful startups and final shutdown.

The exact grant expired at **2026-11-01T21:23:05.308Z**, before these starts. Its
identity, revision, payload digest and expiry remain unchanged. Service::grant
accepts exact existing-payload replay before new-grant expiry validation;
Driver::ensure_binding accepts an already matching artifact/authority binding.
Neither path renews authority. Recovery and automatic idle quanta found no work.
This establishes current read metadata, **not** permission or readiness to execute.
Stored activation controls remain enabled and the UI explicitly warns that these
controls do not establish current grant validity or readiness.

The retained model0.1.0 artifact is
`sha256:0d0918ce4bc486501f851a66c9b1e080d35126061d310d8a5de7701f1bb1c8d3`:
one `main` entry returning `ok` with literal null, no triggers, commands, events,
required capabilities, captured values or dependencies. Its unchanged development
purpose explicitly says startup must not admit it. The operation.get-only grant
references an unused task and is never invoked. The project/workspace record is
an explicitly labelled structural fixture with repository/Graph unavailable;
it conveys no actual source eligibility, Verified observation, ready worker,
task existence, production project installation or execution qualification.

## Actual clients, read APIs and owner handoff

`first37.json` and `restart37.json` retain:

- /readyz reporting source5d445a5 and AQ37;
- current actual Owner bootstrap with catalog/read/inspection/decision caps;
- one current definition and one revision1 activation from `CatalogQuery`,
  `current_namespace_state`, `current_catalog=true`, component APIplaybooks-catalog-1;
- separately labelled startup metadata with `current_catalog=false`;
- empty actual Runs, admissions and Decisions through mounted owner APIs;
- actual core application `aq-private-uds-proof`, bridge and engine clients in
  the same database with Unix client addresses;
- three changing idle-engine query_start samples, without claiming an exact tick
  count or demanding a revision change for an idle no-op.

The first successful upgraded startup used engine incarnation4; graceful shutdown
released its lease/backend and listeners. Restart acquired incarnation5 with the
same pins and exact expired grant. Final graceful stop left owner=false,
owner_backend_pid=null, no core/bridge/engine clients and no4294/4295 listeners.
The retained original state was incarnation2. A diagnostic first upgraded start
used incarnation3 and stopped cleanly: the probe incorrectly expected a blank
core application_name although the retained URL explicitly names it
`aq-private-uds-proof`. That probe-only refusal is preserved in
`initial-probe-diagnostic.json` and its log. No product source, config, artifact,
grant or database restore was used to correct it.

All **17** tracked work/effect counts remained zero before upgrade, after initial
startup, after browser interaction, after stop/restart, and after final stop:
execution attempts; Playbooks admissions/effects/cancellations/outbox;
human/agent Graph operations and agent outbox; mailbox messages/deliveries;
notification messages/deliveries; review subjects; human decision requests/finality;
review outbox; observation events. Metadata migrations, catalog projection
materialization, owner incarnations/recovery metadata and dashboard read snapshots
are real database writes. This is not a claim of zero database writes.

## Actual compiled browser and visual acceptance

The retained probe passed in **3503ms** against real4294 HTTP and the exact served
bundle. It checked one definition and activation, immutable artifact/fingerprint/
source/compiler/scope, purpose, main entry, no declared dependencies, revision1,
stored controls, bound-definition navigation, exact deep-link reload, phone exit
and focus restoration at1440/390/320. It then inspected empty real Runs and Decisions
at all three widths. There were14 catalog browser requests, zero unrelated engine
reads while browsing the current catalog, zero mutations, foreign requests,
page errors or failed API responses. No install/activate/create control is exposed.

Desktop roster/detail and390/320 detail/roster screenshots were inspected,
including full immutable pins and lower controls/entry/dependency panels. Content
wraps without horizontal clipping, and phone exit remains visible. Full-page
phone screenshots include a fixed bottom-navigation capture band at the original
viewport boundary; paired viewport images and checked reflow/focus distinguish
that capture artifact from content loss. This qualifies the delivered read-only
mock06 roster/detail subset, not an editor or workflow execution flow.

The corrected lifecycle took17712ms, including the browser hold and two sets of
idle samples; this is a bounded integration proof, not a throughput benchmark.
`pins.json`, the API observations, browser result, probe, logs, screenshots and
SHA256SUMS bind the record. Private DSN/key bytes and dump contents are excluded.
