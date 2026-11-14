# Profile catalog administration

2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending. The storage paragraph changed.

The editable profile catalog is a `catalog.toml` file and its referenced Markdown
files in the configuration vault. The independently tested `aq-profiles` crate
validates those files and composes their versioned identities. The daemon stores
immutable validated exports and one central activation pointer. There is no
second editable profile database or worker-local activation authority.

Build the inspection CLI with `cargo build -p aq-profiles --bin aq`. Run
`target/debug/aq profile --help` for the validate and explain commands. The
[crate README](../../crates/aq-profiles/README.md) describes catalog fields,
composition snapshots, canonicalization, permission intersections and limits.
The example recipes are composition examples: a valid composition still needs
AQ-20 to choose and authorize a concrete model, account, environment and budget.
An inspection never grants permissions or launches an agent.

All routes below use the human listener, require the current owner role, and
require `X-AQ-Request: 1` for POST requests. They inherit the normal same-origin
and maintenance controls. Worker credentials cannot call these endpoints.

| Route | Behavior |
| --- | --- |
| `GET /api/profiles/catalogs` | Return the central revision and active digest; initially revision zero and no catalog. |
| `GET /api/profiles/catalogs/{digest}` | Return an exact retained validated artifact and its digest. |
| `POST /api/profiles/catalogs/activate` | Atomically validate, retain and activate a catalog with expected-revision fencing and an operation receipt. |
| `POST /api/profiles/validate` | Inspect an explicit exported catalog and optional composition snapshot. |
| `POST /api/profiles/explain` | Require and explain an explicit composition snapshot without admitting execution. |

Activation accepts exactly `operation_id` (a non-nil UUID), `expected_revision`
(the current revision), and `artifact_json` (the exact exported JSON string).
It never accepts a filesystem path, fetch URL, executable hook or caller-selected
principal. Export bytes are revalidated, including their identity hashes.
The success receipt contains `operation_id`, the new `revision`, `active_digest`
and `disposition: "activated"`.

Retry an uncertain activation with the same operation ID and exact payload.
The retained receipt is returned even if later activations have advanced the
revision. Changing the payload under that operation ID conflicts, including
changing whitespace inside `artifact_json`. A new operation with an outdated
expected revision conflicts. Artifact publication, the activation pointer,
immutable identity history and the operation receipt commit in one transaction.

Reusing a definition's identity/version with different metadata or Markdown
conflicts. Publish a new version for changed definitions and update the recipes
that reference them. Removing a definition from a later catalog does not erase
its identity history. Rollback is another expected-revision activation of the
retained old artifact. Invalid edits and failed transactions preserve the last
good active catalog.

Artifacts are at most 2 MiB each (a limit on one input). Old catalogs are a
rolling set: the adapter keeps the active catalog, every catalog a live attempt
pins, and the most recent 128 others within 64 MiB of artifact bytes; activation
receipts are the most recent 4,096 **(proposed defaults)**. An activation deletes
the oldest unpinned catalogs and receipts beyond these, in the same transaction,
and is never refused because storage is full. An exact retry inside the receipt
set returns its receipt; an older one is a new request and conflicts on its
expected revision. Rollback works to any catalog still kept. Identity history
(definition identities and versions) stays; whether its 4 MiB bound may still
refuse an activation is an open question (architecture 0006 §6). The core repository interface
also provides exact-reference retention/release; attempt integration must retain
the frozen catalog and must not resolve an old attempt through the active pointer.

Inspection requests contain `artifact_json`, optionally `composition` and
`graph_access_profile_digest` together. Explain requires both optional fields.
The response explicitly reports `authority_admission: false` and
`selection_pending: true`. Caller-supplied ceilings and available capabilities
are explanatory snapshots. AQ-19/AQ-20 must resolve current trusted project and
provider state before those values can influence admission. The published
CON-02 context-resolving `/api/v1/profiles/*` contracts remain separate from these
explicit-snapshot inspection endpoints.

Catalogs and prompts must contain credential profile references, never secret
values. The inspection API does not read provider credentials or expose catalog
contents to viewers. Concrete provider/CLI compatibility, token estimation,
budget enforcement and running-session activation remain separate milestones.
