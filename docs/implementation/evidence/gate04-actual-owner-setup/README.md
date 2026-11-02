# Actual-owner setup source qualification

Date: 2026-11-01. Source `77ccceb` was independently accepted by Runner; the tiny
compile correction is `ab74e03c1c35361148f9218100686587dc640cac`, based on Root's
union `6727cce`. See [the setup contract](../../GATE-04-actual-owner-setup.md) and
[qualification.json](qualification.json).

This checkpoint qualifies **compilation, local configuration checks and historical
wrapper regression**. It does not claim the new constructor has yet executed
against real Graph +real central Git together, nor a real Codex/task/settlement
cycle. Runner's actual consumer must retain that proof. No additional Graph
process was launched during these checks.

| Check | Actual result |
| --- | --- |
| First compile |failed: AppError is not StdError for anyhow Context; one unused public reexport warning |
| Corrected compile |PASS, 3.59 s |
| Pure explicit actual-setup validation |3 passed, 0 failed/ignored/filtered, 0.01 s |
| Historical native-offer wrappers |4 passed, 0 failed/ignored/filtered, 4.03 s |
| Focused library +two-target Clippy, warnings denied |PASS, 36.81 s |

The compile correction maps the optional diagnostic eligibility error safely and
exposes the same ProfileChoice through a public type alias. No setup data, owner
checks, authority or wire/API shape changed. The failed log remains beside the
passing logs. Both test targets ran with one test thread; compilation used the
coordinated Target2, offline/locked, jobs2, debug0 and incremental0. The target was
explicitly released after the checks.

The pure cases cover canonical catalog/digest/scope, original task versions, fresh
UUIDs, fixed numeric-loopback configuration and no implicit remote/global fallback.
They read explicitly labelled sample immutable catalog bytes locally and create
no owners, registry facts, database, network or process. They are not model/provider
qualification.

The four historical native-offer cases use actual AQ SQL/routers in existing
TestDb UUID databases on the test55432 cluster, with their **explicitly synthetic
Graph and Git peers**. They verify historical behavior only; their test names do
not make those peers real owners for GATE04. Existing TestDb guards own cleanup;
these historical outputs do not retain independent per-database names, and no
such names or separate cleanup proof were invented afterward.

The [source comparison](historical-source-comparison.json) verifies that historical
selection setup after its extracted gate and the full historical native-offer
setup body are byte-identical to Root6727cce. The actual path never calls either
historical constructor or seeds their synthetic model/availability rows.

The new constructor installs the caller's original canonical catalog/full project,
real Graph and source configuration before the first project PUT; reads the actual
ProjectRecord and original bounded ready/context; and leaves last_seen_at, model
credentials/health/node snapshots/registration/decisions/launches unset. These are
reviewed and compiled source properties here. The actual-consumer proof must show
those assertions against the real held owners, actual worker heartbeat/presence,
ordinary single Poll and the separately qualified runtime recipe.

The ordinary worker order remains preflight→real heartbeat→registration/presence→
single Poll. Optional freeze_after_heartbeat is diagnostic, never a second proposal
or driver gate. Env's legacy counter fields are inert on this actual path; the
clock is real Utc::now and actual owners must establish later changes. Original
TaskRevision/Graph observations are never retimed/rebound. Task-only admitted
mutable scope is explicit, without claiming task CAS atomically excludes arbitrary
incoming citations. No canonical state, new service/dependency, broader Graph
precondition feature or production capability changed.
