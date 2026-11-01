# Reviewed inert artifact export and proposed runtime context

Status: artifact export passed; context/configuration below are **proposed, not
applied**. No AQ daemon, namespace, grant, engine key or workflow has been created.

The unchanged `examples/support/development_definition.rs` was copied byte for
byte (SHA256 `692db0cae3ce19143bf166bdc8e5d99cde3da0c32aa4cd00e57202abbe63bad3`)
into an isolated temporary consumer. The production host exposes exactly
`pub use aq_playbooks_model as model;`. The [consumer main](main.rs) supplies that
same model path using `extern crate self as aq_playbook_host` and
`pub use aq_playbooks_model as model`, then uses the example's unchanged main
serialization (only the local definition source path changes). No artifact
fields, defaults, hash or model validation were reconstructed in another language.

The consumer depends only on exact `aq-playbooks-model =0.1.0` from the frozen
`vendor/aq-playbooks-d3bd349` tree and exact `serde_json =1.0.154`. The retained
[lockfile](Cargo.lock) resolves 24 packages. An offline jobs2/debug0 build passed in
7.75s; no host/runner/server/storage module was built. The original typed helper
executed `Artifact::seal` and `Artifact::verify`. Two runs emitted identical 1978
bytes, SHA256 `eb8aec86aafb41ced748d13acb24fd45bf8b5497653b206f6b4f35b28a8d752f`.
Its private runtime file remains mode0600 outside the repository. The artifact
has one Return(ok,null) entry, no commands/events/triggers/dependencies or required
capabilities. [Qualification](qualification.json) records source/binary/output
hashes and exact temporary reproduction paths. The build lease is released.

## Proposed fresh structural context: review before launch

[Pins](proposed-pins.json), [full project record](proposed-project-record.json),
[base configuration](base-config.toml.template), and
[optional configuration](optional-config.toml.template) are review inputs, not a
project installed through its public verification API. The actual Owner remains
unresolved; the optional template deliberately has an invalid Owner placeholder,
zero expiry and absent key files. Nothing should copy it into a live vault yet.
Ports4294/4295 are proposed only; check availability before starting.

The saved enabled project/workspace association is the optional runtime's current
structural startup prerequisite (`Service::initialize` checks only those fields).
The complete DTO avoids breaking project inspection with a partial `{}` fixture.
The reserved repository/node/workspace/task UUIDs assert no corresponding external
record or live worker. The profile/catalog/policy marker values are explicitly
fixture-only, with no artifact registry or profile activation seeded. Repository
and graph observations are `unavailable`, generation is null, and timestamps1/2
are already expired. This does not pass source/selection eligibility and is not a
Verified observation, real Graph task, execution proposal or public installation.
The only unused grant capability is `operation.get`; its required task recipient
does not create a task or mailbox. Nothing invokes it.

After root reviews this context, the bounded isolated phase is:

1. Reinspect the exact existing private PG resource and system identifier using
   the reviewed helper. Verify the pinned state35 binary SHA256
   `8ad0aad8a1cb156660decdebc0fbbef8e4ea04a0e7c80a84c8921bb0681153ad`.
   Create only a new0700 runtime/vault directory inside that private root, and
   write the base configuration0600. Use the existing private AQ Unix URL through
   a child process environment, without echoing it. No canonical config/data is
   copied or restored. Start only the pinned state35 binary on the proposed free
   loopback ports; no source36 migrations or new binary are involved.
2. The base startup creates the fresh AQ schema. GET `/api/me` must bootstrap
   exactly one actual Owner with the configured development email. Record that
   actual UUID and stop AQ gracefully before fixture setup. Require state35,
   exact current database/system identifier, one Owner, zero project bindings,
   and absent proposed namespace. The helper's SQL endpoint is the same isolated
   container/database; no host DSN, arbitrary SQL file or canonical path is input.
3. In one explicitly labelled fixture transaction insert exactly the supplied
   `project_execution_bindings` row, revision1, matching binding/observation IDs.
   Do not seed project history/operations, nodes, catalog activation, attempts,
   decisions, Graph/task state, reviews or observations. Record this one setup
   row separately from runtime effects. Re-read `/api/projects` after startup
   to ensure the actual public decoder accepts the complete record. Do not call
   project eligibility/admission/launch endpoints.
4. Generate two independent32-byte keys once, writing only0600 single-link private
   files. Engine keyring uses `private-uds-engine-1`; bridge key ID is
   `private-uds-bridge-1`. Substitute the actual Owner and fixed now+1hour deadline
   into a new runtime configuration; retain all pins/keys/deadline on restart.
   Keep notifications, terminals, runtime execution and external transports
   absent. The exact reviewed inert artifact is the sole deployment.
5. Snapshot baseline counts after fixture setup: `execution_attempts`,
   `playbook_admissions`, `playbook_effects`, `playbook_cancellations`,
   `playbook_outbox`, `human_graph_operations`, `mailbox_messages`,
   `mailbox_deliveries`, `notification_messages`, `notification_deliveries`,
   `review_subjects`, `human_decision_requests`, `human_decision_finality`,
   `review_owner_outbox`, and `observation_events`. Require zero work counts
   before first optional start. Metadata creation/migrations/Owner/project setup
   are not counted as workflow effects or claimed as zero database writes.
6. Start optional owner, verify actual ready/bootstrap/human APIs, and observe
   at least two1-second idle quanta without manual drive/recovery/admission POST.
   `aq-playbook-owner` and `aq-playbook-bridge` must appear in the same database's
   `pg_stat_activity` with Unix client addresses, while physical cluster identity
   remains the transport pin. Inspect the real labelled startup Definitions and
   empty Runs/Decisions. Startup Definitions are historical loaded metadata with
   `current_catalog=false`. Current0.1.2 cannot qualify the new0.1.3 installed
   catalog/activation inspection; test that separately with its later binary.
7. Gracefully stop/restart using identical config/keys/pins; verify ownership
   release/reacquisition and unchanged work counts. Retain exact binary, config
   hashes (no secrets), endpoint shapes and body-free results. Never manufacture
   workflow history, a decision request, provider output or external delivery.

If any guard fails, stop the isolated AQ process, retain diagnosis and private
files, then use the reviewed exact-resource stop helper only as directed. No
canonical rollback/restore is needed because no canonical resource participates.
This phase still does not authorize a canonical database/optional-owner cutover.
