# Playbooks inspection profile

This slice adds human discovery for the existing configured protected playbook owner. It does not install or activate workflows, dispatch a command, or substitute a synthetic runtime when configuration is absent. API-05/UI-08 remain larger parent tasks; the first live slice is runs, admissions and a bounded timeline.

Routes are mounted as part of `playbooks::api::routes(driver)` only when an actual Driver is supplied:

- `GET /api/playbooks/inspection/runs?limit=25&after=<run-key>&project_id=<optional UUID>`
- `GET /api/playbooks/inspection/runs/{run}?limit=25&after=<receipt-key>&project_id=<optional UUID>`
- `GET /api/playbooks/inspection/admissions?limit=25&after=<request UUID>&project_id=<optional UUID>`

They use current human Owner authorization and the exact configured namespace owner. A project filter must match the namespace project. Owner role/namespace membership are checked again after awaited engine inspection. Revoked business grants do not erase historical reads. These APIs confer no execution authority.

The DTO profile is `playbooks-inspection-1`. Pages include namespace identity/generation/project, server time, optional decimal engine revision, bounded items and an exact key cursor. Engine run/effect/receipt reads reuse its existing public inspector. Raw workflow input, variables, command input/output, private diagnostics and credential/configuration data are omitted. Typed receipt metadata is a further allowlist of sequence, step, time, kind, operation/outcome and child reference. Large revisions and receipt sequences stay strings.

Run detail contains separate run/effect/receipt observations and explicitly sets `observation_is_atomic=false`. Each section remains a separately observed read; no cross-owner snapshot is claimed. Effects show at most 25 operations, with a truncation cursor surfaced as a bounded-view notice. Receipt paging is explicit. Admissions are AQ-owned metadata and carry no invented engine snapshot revision; retained receipt key/digest/activation associations are checked before exposing an engine run link.

Limits: two inspection requests at a time per configured router, three-second total read timeout, at most 25 rows per section and 256 KiB serialized response. The current protected engine response limit also applies. There is no automatic retry, renewal or mutation. Responses use `Cache-Control: no-store`. Keys are namespace-bound filter positions, not access tokens or immutable membership snapshots; every request reapplies current authorization.

The UI uses the negotiated `playbooks.inspect` capability, lists runs in stable key order, filters only loaded rows, and caps loaded run pages at 100 rows. It shows pinned artifact/dependencies, current step/status, public receipt timeline, pending effects and child workflows. Admissions distinguish pending from confirmed engine run. Desktop uses run list/detail columns; phone focus hides the roster and provides an All runs link. Definitions/activation discovery and editing are explicitly deferred rather than inferred from runtime status.

Root integration must enable the capability only when the exact injected Driver belongs to the current namespace owner and routes/adapter are present in the assembled build. Configuration alone is not proof that a Driver was started. Ordinary unconfigured routers retain missing-route behavior and do not return an empty successful list.

`playbook_inspection_contract` generates the machine-readable DTO schema independently of the frozen engine archive. `tests/live/playbooks.spec.ts` exercises an actual configured owner/run and captures desktop/mobile images; it skips explicitly when that installation has no owner or run. A skip is not positive live evidence.

Validation of this source slice: two actual protected-runtime/PostgreSQL/human-route cases passed in 3.64s; two pure bounds/redaction unit cases passed; strict affected library/test/schema-example all-feature Clippy passed in 17.32s; schema generator produced SHA256 `fe3a43277853abde76339e57d15c2dd65d7935536f9f55ede306d7892a0caa4c`. Initial test compilation found a temporary-reference lifetime error in the new concurrent-read fixture; a named path binding corrected it. Initial strict lint found a nested-if style issue; corrected without changing the validation predicate. No provider calls occurred. Logs remain `/tmp/playbooks-server-{actual-tests,unit,clippy-final}.log` and `/tmp/playbooks-schema-build.log` for integration review.

The UI source has passed 12 focused adapter/component tests and typechecking. The actual configured compiled-bundle qualification below now supplies positive browser evidence for the first inspection increment. Refreshed canonical-preview evidence remains a separate root integration gate; an unconfigured live-test skip supplies no positive run evidence.

## Compiled-bundle browser qualification

The [actual configured Playbooks browser proof](../../../aq-dashboard/docs/evidence/configured-playbooks-browser/README.md) qualifies this first inspection increment. A labelled development parent and child are installed/activated/admitted through the real protected owner, both complete, and Chromium navigates the actual compiled page through discovery, immutable pins, four parent receipts, child/parent links, reload, admissions and 390px detail/roster behavior. Browser-only time was 1.237s; the complete isolated engine/daemon/browser case passed in 4.08s. No interception, provider calls or production source changes were used. Strict affected lib/test all-feature lint passed in 1.34s. Original fixture clocks remain fixed and explicitly labelled; screenshots are proof of this isolated composition, not a refreshed live preview or full Definitions/Activations acceptance.

The optional ignored test is `browser::actual_protected_run_and_child_reach_compiled_playbooks_page` in `tests/playbook_activation/browser.rs`; it requires explicit `AQ_TEST_PLAYBOOK_BROWSER_SCRIPT` and `AQ_TEST_PLAYBOOK_DASHBOARD_DIR`. The existing combined assets/policy-owner builder is reused. Full API-05/UI-08 remain open for deferred discovery/actions.
