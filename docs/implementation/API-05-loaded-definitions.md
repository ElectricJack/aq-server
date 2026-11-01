# Loaded startup definition metadata

This next API-05/UI-08 increment delivers read-only **Loaded definitions** over
the actual optional runtime configuration. It keeps the current installed
catalog and Activations tab deferred. The [versioned public contract](../../protocol/playbooks-loaded-definitions/v1/README.md)
defines the narrow capability, closed DTO, bounds and provenance.

Runtime bootstrap extracts metadata only from its actual verified
`Loaded.deployments`, after the bootstrap install/binding calls succeed. The
metadata records configuration, not the outcome/current presence of those
installations. The immutable startup record stores at most four deduplicated
artifact projections and is never updated by later install/activate calls.
It is not a cache of current engine truth. Reads authorize the current human
and exact namespace before and after copying the bounded metadata; no engine
snapshot or component SQL read is performed per request.

The project parameter is a namespace-context guard, not a claim about a
project's installed or active definitions. Deployment configuration validates
project/workspace association, and the wire response retains that namespace.
The UI says Runtime startup configuration and explicitly separates current
installation/activation from the loaded record.

The Definitions tab preserves the approved `06-playbooks.png` roster/detail
composition. Selection is a query parameter on the existing Playbooks route;
phone selection hides the roster and provides All definitions with focus
restoration. Names, entry points and declared dependency aliases wrap at the
model's valid 128-byte bounds. Exact pins and purpose are literal text. No
install, create, edit or activate action is added. Runs/admissions retain their
existing owners and route behavior.

Actual PostgreSQL qualification covers exact source/fingerprint metadata,
UTF-8 truncation, private input/schema/path omission, current-role and foreign
project/namespace refusal, closed query shape, capability mounting and absent
runtime. A real later install leaves loaded metadata unchanged. Holding an
exclusive lock on the component snapshot table does not block this metadata
read. The optional browser fixture uses actual valid 128-byte definition,
entry and dependency names, plus the actual protected runtime and verified
compiled assets; it requires explicit `AQ_TEST_LOADED_DEFINITIONS_BROWSER_SCRIPT`
and `AQ_TEST_LOADED_DEFINITIONS_DASHBOARD_DIR`. It is not a positive browser
claim until the new compiled bundle is exercised and its captures reviewed.

Source qualification: the two actual protected-runtime/PostgreSQL tests passed
in 2.14s. Strict affected library, integration target and schema example with all
features passed in 19.39s. The 33 focused UI adapter/component/regression tests
passed in 2.15s, with app typecheck, probe syntax and diff checks passing. The
published closed schema SHA256 is
`8852409579266e5be4aa965ea4ca1af12c8820224d949db652c89c0503ac37ca`.
An added public-schema assertion initially rejected literal version/source regex
constraints because AQ's finite schema checker deliberately refuses arbitrary
regexes. Exact `const` schemas for version, source and `current_catalog=false`
corrected the contract without expanding that validator. Actual responses now
pass the published schema, including scalar-aligned multibyte truncation.

Subsequent [independent compiled-browser/visual acceptance](../../../aq-dashboard/docs/evidence/loaded-definitions-browser/README.md)
passed on immutable bundle
`8fe4fa06127cc0f3188bb3bbe45fdd6741cbb395f952d27ec7d7dd29b6b3628c`
in 2.89s (1207ms browser). A capture-only second run passed in 2.65s to inspect
lower phone panels. Actual 128-byte names/entries/dependency aliases, exact
metadata, reload, 1440/390/320 layout and phone exit/focus were independently
reviewed. Definitions made no engine inspection requests; a final explicit Runs
navigation made one intended read. Mutations, foreign requests and page errors
were zero. The complete UI `tsc -b` check also passes after two test-only typing
fixes. Canonical runtime activation/promotion remains a separate operator gate;
the current TCP-only PostgreSQL deployment is not a valid Unix owner transport.

## Follow-up for the current installed catalog

The frozen runner inspector exposes only Runs, Effects and Receipts. Its
persisted State has artifacts and activation bindings, but there is no bounded
public catalog query. The existing known-binding read takes a full snapshot;
using it repeatedly is not a new bounded catalog API. AQ must not bypass the
component boundary with direct database reads or alter the frozen vendor archive.

PostgreSQL storage acquisition holds a session advisory lock. Its CAS checks
the live advisory lock, owner, incarnation, backend PID and expected revision
under the namespace row lock. These fence compliant concurrent writers, but a
process-local current-catalog projection would need to detect connection loss
and a successor owner; serialized local commands alone do not establish
freshness. Therefore this increment makes no current-catalog claim.

Full installed Definitions and Activations require component-owned bounded
list/detail inspection, safe persisted projections maintained with authoritative
CAS, and tests for takeover/restart/unknown commits, namespace scoping and
protected artifacts. Publish a new aq-playbooks version and exact consumer pin
for that extension. Show only represented binding revisions/controls; do not
invent activation timestamps, readiness, live grants or human gates. API-05 and
UI-08 remain in progress until their full declared scope passes.
