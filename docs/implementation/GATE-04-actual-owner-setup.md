# Actual owner setup before native selection

Status: source-only test integration from Root union6727cce. No new database,
Graph/Git/worker/provider process, build or canonical effect has run for this seam.
The already-qualified real Graph helper5fec17c remains a separate held child/DB
owner. Runner owns its actual Codex/FD transport/runtime recipe; this change owns
only the common actual project setup path. Historical synthetic fixtures remain
explicit and unchanged apart from sharing their existing serialization mutex.

## Caller contract

`common::selection::ActualSetup` (also accepted by
`common::native_offer::setup_actual`) contains:

- declared `enrollment_worker_version` from the caller's pinned worker recipe;
- typed `ProjectBinding` with fresh project/repository/workspace identities, exact
  caller policy/ceilings/context and allowed profile/environment IDs;
- exact canonical `catalog_artifact_json`, whose digest must match the project;
- actual `AgentAccessConfig`, including the real Graph service actor/token FILE;
- original actual `aq_admission::TaskRevision` from the held Graph fixture;
- actual `SourceRetentionConfig` for the already-running central aq-git owner.

These are test inputs, not launch authority. The outer case retains real process,
repository/checkpoint, executable/config and Graph mutation receipts. It creates
real Graph and central Git before common setup, and keeps their guards alive until
settlement/cleanup. No global environment mutation or fake→real replacement occurs.
The constructor does not choose an example catalog, `model-a`, placeholder source
commit, runtime qualification or availability observation on behalf of the caller.

Pure validation checks canonical artifact/digest/scope/fresh UUIDs and fixed
numeric-loopback Graph configuration before creating an AQ fixture database. The
three pure tests use explicitly labelled immutable sample catalog bytes only to
exercise local validation; they launch no owners or runtime/provider qualification.

## Setup ordering and retained facts

1. Create only fresh disposable AQ database/Owner/enrollment state through existing
   owner functions. The enrollment credential is a fixture provisioned credential,
   not proof of a running worker. Map caller-declared worker groups to that fresh
   enrolled Node. Do not set last_seen_at or create protocol2 registration/inventory.
2. Construct real Graph and source-retention adapters before the first project
   observation. Install the caller's canonical catalog as the initial activation
   (revision0→1); never activate synthetic/example/model-a metadata first.
3. PUT the exact caller ProjectBinding through the actual admin owner. That owner
   obtains Graph workspace and Git repository/generation observations from the
   configured real endpoints before committing a Verified project record. Read
   back the real ProjectRecord; no fake BindingObservation or SQL seed is inserted.
4. Obtain actual bounded ready/context reads for the ORIGINAL task pin. Require
   exactly that ready task, no page continuation/truncation, the same actual root,
   and zero neighboring nodes/edges/pinned neighbor revisions. Never trim required
   context, substitute a newer revision or restamp a saved response.
5. Assert that last_seen_at remains absent and credential definitions, intelligence
   observations/node snapshots/decisions, execution registrations and launches are
   empty. Return the actual owner data before any admission.

`ActualSelectionFixture` exposes Env, actual ProjectRecord/catalog activation
revision, actor/lineage, immutable starting TaskRevision and the actual stamped
ReadyPage/TaskContext. It deliberately has no old `Case.input`/SelectionInputs,
synthetic credentials, fake health or manual runtime registration.

The returned graph reads are retained inspection evidence, not authority to skip
later current reads. They are ordinary schema8 Graph API observations, not the
unreleased committed-observation replay protocol or wake receipts.

## Ordinary worker ordering stays authoritative

Runner installs its separately pinned finite-peer intelligence/runtime/context
recipe from real preflight evidence. The ordinary `run_once` sequence remains
preflight→real connector heartbeat→registration/presence→single Poll. The heartbeat
is actually awaited by `runner/heartbeat.rs` before registration/Poll. This fixture
does not make a fake heartbeat, freeze time, add another Poll/proposal driver, or
prepare an offer to work around an ordering assumption.

Optional `freeze_after_heartbeat(ActualProfileChoice)` is a diagnostic call to the
existing production project eligibility owner AFTER a genuinely observed heartbeat;
it never records liveness, installs registry facts or grants work. It is not required
as a new driver gate: ordinary Poll performs its own fresh eligibility/selection.
A constructor cannot return a pre-heartbeat fake eligible node. If the worker has
not heartbeated, eligibility must remain refused under the ordinary owner rules.

First-pilot admitted mutable Graph scope remains exactly {task}, with no required
neighbors/dependencies. Caller immutable catalog/source artifacts are separate pins.
Task CAS does not atomically assert absence of arbitrary incoming citations; broader
input/absence policies remain unsupported pending the separately deferred Graph
transaction assertion owner. The initial fixture pin is never rebound; the actual
published/execution task must join the caller's original task and source receipts.

## Checks still required

Compile/pure/strict and relevant historical wrapper regression checks need a target
lease; both Root SERVER and Human Target2 were active during source preparation.
No additional Graph process is started merely to decorate this source work with
another component smoke. A changed-consumer proof, if required, must be reviewed
and join real Graph +real central Git before first project verification. The actual
Codex composition is Runner's consumer and should carry the existing qualified
helper's ordered cleanup and teardown timing limits. No source-only claim is a
successful model cycle or B1/B2/C settlement.
