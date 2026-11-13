# Playbook Authoring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent, working with documentation and the engine's tools inside an internal-work run, turns any stored Markdown source into a playbook artifact that the server checks once at save, installs and activates. The routing document in Settings is the first source, and its playbook routes every task.

**Architecture:** The engine (aq-playbooks) gains `lookup` and `let` expressions, a typed read of an asynchronous answer, host catalog 2.2.0, a draft format that uses contract names, a simulation library with cases, and agent-facing CLI commands (`check`, `try`, `compare`, `save`, `ask`, `unable`). These run offline in a scratch workspace. The server (aq-server) gets internal work in a new agent mode, a new `aq-playbook-authoring` crate (sources, runs, workspace building, save-time checks, install, activate, rollback, HTTP), Host9 adapters for task facts, routes and structured judgments, the server-originated `aq.work.route_needed`, and the W6 failure path. The deterministic routing decision table and its compile-on-save are deleted.

**Tech Stack:** Rust 2024 (both repos), sqlx/PostgreSQL migrations, axum, serde/serde_json, jsonschema 0.61 (already in aq-internal-work), the Claude Code and Codex CLIs as harness natives, React/TypeScript (aq-dashboard).

**Spec:** `docs/specs/0013-playbook-authoring.md` at `c656127c` (binding). Also `docs/specs/0011-routing-policy.md` (§3 and §4 stand), `docs/specs/0012-internal-work.md`, and the engine's `docs/specs/0001-aq-playbooks.md` §4–§7, §15–§16.

---

## Spec check

Every claim below was checked against the code on 2026-11-13 and cites file:line. "Server" means `/home/jkern/dev/aq-platform/wt/status-sync-server`. "Engine" means `/home/jkern/dev/aq-platform/wt/status-sync-playbooks`.

### NEEDS RULING

**R1. One playbook namespace has to route tasks of every project (§7, §6).** The playbook owner is configured once, for one namespace and one project (server `playbooks/config.rs:30-43`, `service.rs:22-29`). Grants must name that project (`service.rs:411-417`). The host adapters refuse any task outside it (`host_adapters.rs:1052-1056`, `namespace_work`). Routing is installation-wide (spec 0011 §2: one global policy). A Host9 profile also requires a new namespace generation, because the namespace row pins the profile's registry digest (`service.rs:308`).
- (a) **Recommended.** In the configured namespace, the three new commands (`work.read`, `work.route`, `judgment.assess`) accept a task of any project whose binding is on the automatic router. All other commands keep the namespace's project check. A new config flag gates this: `installation_routing: true` in the playbooks section, valid only with `host_v9`. The live namespace moves to `host_v9` with a fresh generation (runbook F2). Host8 coding artifacts keep working there, because catalog 2.2.0 only adds contracts (item 7).
- (b) A second namespace that the server creates and manages for authoring sources. It needs a second engine owner thread and driver in the aq-server lib, which is at its line ratchet.
- (c) One namespace per project. There is one config section, so this cannot be built without a config redesign.

**R2. Automatic activation needs a grant that the server holds on the owner's behalf (§4.5).** Activation needs a grant id and revision (`driver.rs:482-508`). Only the namespace owner may create a grant (`service.rs:411-414`). A grant expires within 24 h (`service.rs:443-446`), and every effect re-checks expiry at dispatch (`service.rs:586-597`).
- (a) **Recommended.** For each source the server creates a grant as the namespace owner, using the source record's capabilities. Every 12 h it issues a new grant (24 h expiry) and re-activates the current artifact under it. That re-activation goes to the engine directly, not through the rollback ledger (`artifacts.rs:370-405`), so renewals never fill the rollback stack. Runs admitted under the previous grant have a 12 h overlap to finish. Table `playbook_source_grants` (Task C1).
- (b) A non-expiring source grant. This changes the 24 h rule for every grant.
- (c) The owner grants by hand. This defeats "takes effect at once".

### Resolved by the plan

**1. §5.1 lookup.**
- *Where it fits.* A new `Expr::Lookup { key, cases: Vec<LookupCase{when: Value, then: Expr}>, default }`, beside `Expr` at model `lib.rs:248-306`. Evaluation goes in `eval_inner` (`:823-1056`) and typing in `infer_expr` (`:2315-2494`). The runner never matches on `Expr`; it calls `eval` through `known_expr` (runner `lib.rs:1550-1556`). So this is a model-only change, which can run beside the live-state plan.
- *Types.* Each `when` literal's `literal_schema` must merge with the key's type, by the same rule as `Eq` (`:2371-2378`). The result type is `union_schemas` over every `then` and the default (`:2305-2314`). It fails past 16 alternatives. String results merge into one enum (`merge_schema` `:2234-2250`), so `level` is typed as the set of levels the table can yield.
- *Evaluation.* An `Unknown` or `Missing` key is returned unchanged, keeping the conservative evidence rule (`:894-897`). Otherwise the first canonical-equal `when` is chosen, and only its `then` is evaluated.
- *Cost.* At most 256 cases. Each `when` is at most 1 KiB canonical, and keys are distinct. Each lookup charges `cases.len()` to the element budget (`EvalLimits.max_elements`, `:334-342`).
- **Spec gap: lookup alone is not enough.** There is no `if`, no `max`, no variable, and a `Decide` binds nothing (`:547-549`). So "at least Standard" has to be written as `max(a, b) = lookup(lt(a, b), [{when: true, then: b}], default: a)`, and each operand appears twice. The routing level nests about four such `max`/`min` steps over a nine-case rank table. That repeats the hint lookup about 8 times and makes the route input well over 50 KB. Add `Expr::Let { bindings: [{name, value}], body }`: sequential, single-assignment, and lexical like `Map.item` (`:2422-2426`). It is also model-only.

**2. §5.2 per-outcome wait vs. the named join.** A per-outcome `Wait` would need new `EventContract` fields (an outcome field, plus a schema per outcome) and new `Wait` edges. It would also change event fingerprints unless every new field is skipped when empty. The named join already does the job:
- `member_outcomes` (`:2955-2979`) types a join member that is an Invoke's handle by the command's business outcomes.
- `join_schema(step, true)` (`:2987-3058`) narrows that type on `completed` when the join mode is `all` or `any_accepted` (`:3004`).
- The runner records an async completion as the intent's result (runner `lib.rs:2850-2958`) and builds `{participant, outcome, output}` for it (`:2360-2431`). FORMAT.md documents `joined.review.output.subject` reads.

One mismatch: the model accepts the member path `["handle"]` (`:2956`), but the runner treats a bare string as a child id and refuses it (runner `:2092-2099`). Only the path `[]` works (`{"handle": h}` → `:2100-2108`). **Recommendation:** use the join. Restrict the model to path `[]`, test the path end to end, and teach it in the reference (Task A2). No `EventContract` change.

**3. §5.3 structured answers.** An output type that depends on an input value cannot be expressed. `CommandContract.outcomes` are fixed schemas (`:361-378`), and the registry must be byte-equal to the pinned catalog (server `artifacts.rs:281-301`). Literal-dependent typing would cost a model extension and buy almost nothing: `merge_schema` of two strings never fails (`:2234-2250`), so an enum-typed label does not catch typos either.

**Buildable design:** a new command, `judgment.assess`.
- The request names `fields: [{id, description, choices}]` (at most 16 fields, 64 choices each, 128 B per choice) and `flags: [{id, description}]` (at most 32).
- The host asks internal work with `AnswerShape::Json` and an object schema it builds from the request (`{<field id>: enum, <flag id>: boolean, rationale}`), so the model answers by name.
- The host returns a fixed bounded output: `labels` (strings in field order) and `flags` (the ids that hold).
- Playbooks read `answer.c.output.labels.0` (array-index paths: `schema_path` `:2908-2912`, `path_value` `:813`) and `contains(answer.c.output.flags, "narrow")`.
- The whole request is at most 64 KiB (`max_payload_bytes`).

`judgment.request` (2.1.0) is unchanged.

**4. (Replaced.) The agent's draft, and what the CLI offers and must add.**
- *Draft format* (`draft.json`). This is `Playbook` (`:521-530`) with three differences:
  - `command`, `event` and trigger `event` are contract **names** (strings), not `ContractRef {name, fingerprint}` (`:421-424`);
  - `id`, `scope` and `required_capabilities` are absent; the tool writes them from `record.json`;
  - `limits` is optional (default `Limits::default()`), and `source_map` (step → line in `source.md`) is optional.

  The tool fills fingerprints from `catalog.json`, which the server filtered to what the source may use. It adds each async command's reconciliation command and completion event (the way `coding.rs:431-449` does today) and unions their capabilities. It writes the `---aq-playbook` header (item 5) and captures `facts.json` as definitions by digest (`CapturedDefinition` `:644-648`). A name outside the catalog gets a diagnostic that lists the allowed names. `Proposal.questions` and `unsupported_features` (compiler `lib.rs:9-14`) are removed, and the `ask` and `unable` tools replace them.
- *What the CLI has today* (engine `crates/aq-playbooks-cli/src/main.rs:29-178`):

  | Subcommand | What it does today |
  | --- | --- |
  | `validate`, `compile` | Take a full `CompileInput` with fingerprints |
  | `diff` | Two sealed artifacts |
  | `commands`, `events` | Default to the synthetic 1.0.0 catalog (`:306-309`) |
  | `interfaces`, `surface` | Synthetic pilot surface |
  | `examples` | Rust-built pilots, written out as scenarios |
  | `simulate` | A `Scenario` with full artifacts, fingerprinted `EventEnvelope`s and `DispatchResult` queues |

  `simulate` does not record command inputs (`simulation.rs:46-62` keeps intent ids only), so a case cannot assert "the route written was X". It does not synthesize async completions either.
- *What must be added* (Tasks A3, A5, A6). A workspace convention, plus these commands:
  - `check` and `seal` replace `validate`/`compile` in `CompileInput` mode;
  - `try CASE` records calls with inputs, synthesizes async completions, and reports an unmocked command;
  - `compare` replaces `diff`, against `previous.artifact.json`, in owner terms;
  - `facts`;
  - `save`, `ask` and `unable` write `out/outcome.json`;
  - `commands` and `events` default to the workspace catalog.
- *Guide and reference* (Task C5).
  - `LANGUAGE.md`: every operation and expression with JSON, typing rules, limits, and every `ValidationIssue.code` with what to do about it.
  - `GUIDE.md`, the extended skill: the workspace and tools; patterns (read → decide → write, ask → join → read labels, lookup tables, `let`, max/min, conditional lists with `filter`, reasons with `filter`/`map`, fallback paths, stale and refused outcomes); how to cover the document's situations with cases; when to `ask` and when to give up with `unable`; owner-language questions.
  - Two complete small worked workspaces that CI checks and tries. None exist today: the pilots are Rust builders (`pilots.rs:86-201`).

**5. §4.3 names without fingerprints under `deny_unknown_fields`.** This is sound as a JSON pre-pass. `draft::fill` rewrites only the three known string positions into `{name, fingerprint}` objects, then deserializes into the strict model types. Any other unknown field is still refused, with serde's line and column in `draft.json`.

**6. §4.4 cases.**
- What `Scenario` (`simulation.rs:34-44`) can express: a trigger start (`Action::Event` + `fanout`, `:134-139`), and queued returns per command name.
- What it cannot: assertions on calls; async answers given as plain outcomes (an `Accepted` handle followed by a completion event); a run outcome after spec 0003 (finished runs leave `snapshot()`; live-state follow-on item 2); detection of an unmocked command (it becomes `Unknown`, `:59-61`).
- Task A5 adds `Case {situation, start, returns, expect {outcome, calls[{command, input ⊆}]}}`, `run_case` and the `pending` return (no answer, so the join times out).

**7. §6 catalog 2.2.0 and Host9.** The spec words items 1, 3 and 5 as changes to `context.read`, `work.change` and `judgment.request`. That changes three fingerprints that every Host8 coding artifact pins (D11 digests, `docs/implementation/D11.md:189-206`). It also cannot type per-view facts, because `context.read` has one output schema for all views (engine `contracts/aq-host-2.1.0/registry.json`), and paths cannot traverse `Nullable` (`schema_path` `:2921-2923`). `work.change.value` is a 4096-byte string, so it cannot carry a typed route.

**Shape:** 2.2.0 is 2.1.0 byte-identical, plus these additive contracts (Task A4):
- `work.read` 1.0.0: task facts. All fields are required; `""` or `0` means none. No priority and no load, by construction.
- `work.route` 1.0.0: `level`, `harnesses`, `providers`, `prefer`, `hold`, `reasons[]` and `classification`.
  - The spec's route has no way to say "wait for Codex", so `hold` is added for the Art sentence.
  - "A reason" becomes `reasons[{code, detail}]`, so every raise or lowering can be recorded.
  - "An optional minimum level" is dropped: the playbook computes floors with `lookup`/`lt`.
- `judgment.assess` 1.0.0 (item 3), with event `aq.judgment.assessed`.
- Event `aq.work.route_needed`.

§6.2 (`execution_options` at run time) is not built. The facts are captured as definitions when the playbook is authored, and `work.route` re-checks the route against Settings when it is written. A save also recompiles when the facts change (Task D2).

**Adoption:** `CatalogProfile::Host9` (number 9) loads `vendor/aq-host-catalog-2.2.0`, pinned to its fingerprint like Host8 (`review_domain.rs:31-56`). `is_host2` includes Host9. Host9's capabilities are Host7's plus three (`:58-71`). The coding builders keep the Host8 registry. Their contracts are identical in 2.2.0, so they validate (`artifacts.rs:291-299`).

**Projects and binding:** R1. The routing playbook's engine binding is its playbook id `aq.routing`. It is activated by the authoring service under the R2 grant. Spec 0011's "automatic-router binding" stays a project execution binding, which the adapters consult.

**Purpose (§6.5):** a judgment's purpose comes from its source record (`routing` → `routing_classification`, others → `playbook_call`). It is found through the artifact digest of the run in `playbook_authoring_runs`. `InternalJudgments` takes one purpose per service (`internal_judgments.rs:9-17`), which does not fit one namespace that serves both kinds of source.

**8. §7 a run from a server-originated event on a host profile.** Outbox delivery (`driver.rs:676-748`) and trigger fan-out (runner `lib.rs:827-878`) do not depend on the profile. Only the domain collectors are limited to Monitoring6 (`triggers.rs:31-36`, `driver.rs:666-675`). Two gaps:
- A host-profile outbox event that no installed artifact declares is retired with zero destinations (`driver.rs:681-704`). `declared_events` is filled only on `install` (`:425-467`), and that happens at startup only for config deployments. An artifact installed by the authoring service would therefore stop receiving `route_needed` after a restart. Task C4 re-installs each active authored artifact at start; this is content-addressed and a no-op in the engine.
- No outbox writer exists for host profiles (`queue_occurrence` `triggers.rs:200-219` is the pattern). Task B5 adds `queue_host_event`.

**9. §4.5 grant:** R2.

**10. (New.) Internal work in agent mode.** Today a run is one prompt with tools off:
- Claude: `--tools ""` (`internal/claude.rs:113-130`).
- Codex: `--sandbox read-only` (`internal/codex.rs:64`, `subscription.rs:322-347`), in an empty scratch directory (`internal.rs:520-560`).
- The answer is parsed from stdout and checked by `validate::answer` (`internal.rs:449-470`).
- The deadline is at most 900 s (`aq-internal-work/src/lib.rs:21`). Each candidate gets an even share of what is left, with a 30 s floor (`:28-40`).
- One Codex run at a time holds the login (`internal.rs:252-258`).

Agent mode (Tasks C2, C3):
- *Request.* `Request.agent: Option<AgentWork {files: BTreeMap<path, text>, max_turns}>`, with `#[serde(default)]`, so stored rows still decode. The answer shape is the JSON Schema of `out/outcome.json`. `internal_tasks.workspace_kind` gains `'agent'` (new migration; 0119 is frozen).
- *Scratch.* The host writes `files` into the run's scratch directory, which becomes the workspace. The agent writes drafts and cases there. The host reads `out/outcome.json` (at most `max_output_bytes`) as the raw answer.
- *Tools.* The engine CLI `aq-playbooks` is the only tool, and it is **offline**. Discover, facts, check, try and compare read the workspace files. `save` runs the same checks the server runs (item 3 of §4.3), then writes the outcome; `ask` and `unable` write the outcome.
  - Claude: `--tools "Bash,Read,Write,Edit,Glob,Grep"`, Bash allowed only as `aq-playbooks …`, Web tools disallowed, `--max-turns`, `--output-format stream-json --verbose`.
  - Codex: `--sandbox workspace-write` with network access explicitly off.
  - The native's environment is cleared (`subscription.rs:409`) and has `/usr/local/bin` on `PATH`. The `aq-playbooks` binary is built from the vendored engine in `Dockerfile.supervisor-host`.
- *Credential: none.* No tool reaches the server, so there is no run credential to scope or leak. The server re-checks the outcome after the run (Task C4), and refuses only on drift: facts or grant changed. The alternative, a per-run bearer token that the host mints into the private home (the supervisor pattern, `adapter.rs:26-56`), is not needed, because nothing in the tool list must be online.
- *Result.* The internal task's answer is `{"kind": "saved" | "question" | "unable", …}`. With no outcome file the run fails `invalid_output` ("the agent ended without saving, asking or giving up").
- *Deadline and budget.* Agent requests allow up to 1 800 000 ms. A candidate's budget is what is left minus 300 000 ms for each later candidate, with a 300 000 ms floor, so a fallback still gets 5 minutes.
- *Codex lock.*
  - At most one agent run per host at a time, so one of `MAX_RUNS = 2` slots (`internal.rs:180`) always serves answer runs.
  - A Codex agent run is claimed only when no queued blocking task has Codex as its last candidate.
  - While it holds the login, `pass_busy_codex` (`:300-324`) moves tasks that have another candidate on. A task whose only candidate is Codex waits up to 30 min. Routing still routes on its fallback path, at the judgment join's deadline.
  - Recommended override in Settings: `playbook_compilation` → a Claude-first profile.
- *Transcript.* The host keeps, per candidate, the harness's event stream: tool calls and results clipped to 2 KiB each, assistant text, at most 256 KiB, redacted the way runs already are (`claude.rs` `redact`). It is stored in `internal_task_transcripts` and shown in the run view (Task E2).
- *Supervisor session instead?* A supervisor turn is a mailbox conversation that reserves a provider and profile cell, and it carries a graph-wide credential (spec 0012 §4, §8). It has no first-class "outcome" either. **Recommendation:** internal work agent mode, as the spec calls it: it is never queued, uses the same profiles and failover, and has no credential.

**11. (New.) Inventory of existing compile-related code.**

| Code | Lines | What it does | Fate |
| --- | --- | --- | --- |
| engine compiler `validate`, `compile` (`lib.rs:103-164`) | 62 | Header + structure checks, seal | REPURPOSE → `check`, `seal`, server save. Input comes from `draft::fill` (A3) |
| engine `parse_source_header`, `normalize_source` (`:52-87`) and the `---aq-playbook` header | 36 | Source identity | REPURPOSE. An artifact still needs `SourceBinding` (`:631-635`), checked at `:1352-1379`; the source digest ties the artifact to the document revision. `draft::source_text` writes the header; the agent never does (A3) |
| engine `semantic_diff`, `ChangeKind` (`:165-229`) | 65 | Diff kinds | REPURPOSE → `compare` and the owner's diff label (A3, A5) |
| engine `diff` alias (`:230-232`) | 3 | Duplicate | REMOVE in A3 |
| engine `Proposal.questions`, `unsupported_features`, `Severity::Question` (`:9-14`, `:26-30`, `:112-128`) | 30 | Model questions | REMOVE in A3; `ask` and `unable` replace them |
| engine `source_map` (`Proposal`, `ArtifactContent`) | ~20 | Step → line | REPURPOSE: draft lines in `source.md`, shifted past the header; diagnostics carry the line; the owner view names the sentence (A3, E1) |
| engine model `validate_structure` + typing (`:1320-3059`) | ~1700 | Every structural and type rule | REPURPOSE → `check`, save, install. Diagnostics already have `code` and `path`; A3 adds the line, and C5 documents each code |
| engine model `Artifact::seal`, `verify`, `from_canonical_bytes` (`:728-776`) | 49 | Sealing and integrity | REPURPOSE: seal through `draft::seal`; verify at save and install, unchanged |
| engine CLI `validate`, `compile` (`main.rs:318-340`) | 23 | `CompileInput` mode | REPURPOSE → `check`, `seal` on a workspace (A5). The `CompileInput` mode goes |
| engine CLI `diff` (`:341-343`) | 3 | Artifact diff | REPURPOSE → `compare` (A5) |
| engine CLI `simulate` subcommand | ~10 | Raw scenarios | REMOVE in A6. The library `simulate` stays behind `try` and `run_cases` |
| engine CLI `examples` (`:354-372`) | 19 | Writes pilot scenarios | REPURPOSE: writes the worked workspaces (A6) |
| engine CLI `commands`, `events` (`:347-348`) | 10 | Discover | REPURPOSE → discover; default is `./catalog.json` (A5) |
| engine CLI `surface`, `interfaces` (`:46-47`, `:349-353`) | 15 | Synthetic surface; callable entries | REMOVE in A6. Authoring sources are offered no callable dependencies |
| engine `cli/src/simulation.rs` | 183 | Scripted simulator | REPURPOSE → crate `aq-playbooks-simulation`, which records calls (A5) |
| engine `cli/src/fake_host.rs` | 1777 | Stateful synthetic host, used only by tests | REMOVE from product code: moves to `crates/aq-playbooks-cli/tests/support/` (A6) |
| engine `cli/src/pilots.rs` | 1078 | Rust-built pilots and scenarios | Routing pilot (`:613-700`) REMOVE in A6 (superseded; teaches the attempt-keyed pattern spec 0011 §1 condemned). The rest moves to `tests/support/` (A6) |
| engine `cli/src/catalog.rs` | 128 | Catalog registries | REPURPOSE: `catalog_v2_2()` is the catalog the server filters (A4) |
| engine `skills/aq-playbook-author/SKILL.md` | 20 | Authoring skill | REPURPOSE: becomes the authoring guide shipped to the agent (C5) |
| engine `crates/aq-playbooks-model/FORMAT.md` | 60 | Format rules | REPURPOSE: updated for lookup, let and join (A1, A2); linked from `LANGUAGE.md` (C5) |
| server `aq_routing::{policy, plan, compile, fixtures}` | 1579 + 1661 + 735 + 238 | Decision table, planner, prompt, fixture | REMOVE in D1. `compile::offers` (`compile.rs:341-356`) moves into `classify` |
| server `aq_routing::classify::{Options, Offer}` | ~25 | What Settings offers | REPURPOSE → the routing facts provider (D2) |
| server `aq_routing::classify::{Classification, Risk, Flag, schema}` | ~90 | Old answer shape | REMOVE in D2 (`Route.classification` becomes JSON) |
| server `routing/compile.rs` | 403 | Compile-on-save bookkeeping | REMOVE in D1. `options()` moves to `routing.rs` (D1); the bookkeeping pattern is rebuilt in C1 |
| server `routing/http.rs` policy and switch handlers (`:22-48`) | 27 | View and switch | REPURPOSE → routing source view and switch (D2) |
| server `tests/it_profiled/profile_catalog/routing_compile.rs` | 1016 | Tests of the above | REMOVE in D1 |
| server `playbooks/coding.rs` `Builder::seal` (`:420-520`) | ~100 | Ad hoc registry, capability and source filling | REPURPOSE in D4: registry and capability derivation through `draft::retain_registry`; digests unchanged |
| server `playbooks/coding/pilots.rs` `routing()` (`:52-206`) | 155 | Host8 routing pilot | REMOVE in D4. `routing_scale.rs` and `real_adapter_pilots.rs` move to the authored fixture |
| server `playbooks/artifacts.rs::validate` (`:281-301`) | 21 | Contracts equal the pinned catalog | REPURPOSE: called on the save path (C4) |
| server table `routing_policies`; column `routing_state.active_policy_digest` (0120) | — | Old compiles | REMOVE: migration 0121 drops them (D1) |
| server table `route_needed_emissions` (0120) | — | Unused today | REPURPOSE: emission throttle (B5) |
| server dev-dependency `aq-playbooks-compiler` (`Cargo.toml:104`) | 1 | Unused in source | REPURPOSE: a normal dependency of `aq-playbook-authoring` (C1) |
| dashboard `RoutingPolicyEditor.tsx` `CompiledPolicy` (`:13-53`), `api/routing.ts` active/latest adapters, `mocks/intelligenceStructure.ts` routing fixtures (`:30-60`) | ~140 | Old view | REMOVE and replace (E1) |

Task F1 runs `scripts/check-authoring-inventory.sh`. It checks that no REMOVE symbol remains, and that each REPURPOSE symbol is called from a CLI tool or the save path.

**12. Smaller corrections.**
- D11.md is in the server (`docs/implementation/D11.md`), not the engine.
- Spec §8 keeps `routing_policies`. The owner's later rule ("nothing unused") wins, and 0121 drops it.
- Acceptance 7's measurement (`tests/it_playbook/routing_scale.rs:86`) uses the Host8 pilot. It moves to the authored routing fixture (D4).
- New aq-server crates may not add `tests/*.rs` targets, because the test-binary budget only goes down (`build-budget.toml:12`). Their tests are unit tests, or live in the existing `it_*` binaries.
- aq-server lib is exactly at its ratchet (110267, `scripts/build-budget.py` OK). D1 frees about 430 lines, which later glue spends. Logic goes into new crates.

### Appendix: what the routing document should become (feasibility proof)

**Facts** (definitions, captured by digest; owner's installation, from `aq-routing/src/fixtures.rs:14-24, 136-166`):
- `levels`: `simple-low`, `simple-high`, `standard-low`, `standard-medium`, `standard-high`, `complex-low`, `complex-high`, `deep-low`, `deep-high`, in order;
- `offers`: `{anthropic, claude}`, `{openai, codex}`, `{google, gemini}`, `{ollama, opencode}`;
- `harnesses`, `providers`.

**Entry `main`:**
- Input: `{task: {id, revision}}`.
- Outcomes: `routed {level}`, `skipped {}`, `unservable {}`, `failed {}`.

**Trigger** `route-needed`: on `aq.work.route_needed`, with input `{task: event.subject}`.

**Steps:**
1. `read`: Invoke `work.read {subject: input.task}`, bound as `task`. `observed` → `ask`; `refused` → `skip`; `on_error` → `failed`.
2. `ask`: Invoke `judgment.assess`, bound as `j`.
   - `question`: the "By risk" section, the kind list, and the definitions of "narrow" and "covered by a test", in the document's words, plus "go by the riskiest; when you can't tell, answer medium".
   - `context.items`: the task's title, description and the kind the filer gave.
   - `fields`: `kind` (feature, bugfix, refactor, test, docs, chore, sync, spec, design, plan, review, research, analysis, art, repair, unknown) and `risk` (low, medium, high, very_high, unknown).
   - `flags`: `narrow`, `covered_by_test`, `needs_deep_reasoning`, `clearly_harder`.
   - `accepted` → `await`; `assessed`, `failed`, `refused` → `assume`; `on_error` → `assume`.
3. `await`: Join `members [ref j]`, `member_keys [c]`, mode `all`, `accepted_outcomes {assessed}`, deadline 180 000 ms, remaining `cancel`, bound as `answer`. `completed` → `route`; `no_match`, `timeout` → `assume`.
4. `route`: Invoke `work.route` with input `let(…)`.
   - Bindings:
     - `kind = answer.c.output.labels.0`, `risk = …labels.1`;
     - `nt = and(contains(flags, "narrow"), contains(flags, "covered_by_test"))`;
     - `R(level)` is a nine-case literal rank table;
     - `hint = lookup(task.facts.level_hint, R…, default 0)`;
     - `base = lookup(kind, {chore|sync: R(simple-high); design|review: R(complex-high); spec|plan: lookup(deep, {true: R(deep-high)}, R(complex-high)); analysis: lookup(deep, {true: R(deep-high)}, R(standard-high)); research: lookup(harder, {true: R(complex-high)}, R(standard-high))}, default R(standard-high))`;
     - `cap = lookup(kind, {test|docs|chore|sync: R(standard-high); feature|bugfix|refactor: R(complex-high)}, default R(deep-high))`;
     - `asked = lookup(lt(0, hint), {true: min(hint, cap)}, base)`;
     - `esc = lookup(and(eq(kind, "bugfix"), lt(0, task.facts.charged_failures)), {true: max(asked, R(complex-high))}, asked)`;
     - `floor = lookup(risk, {medium|high: R(standard-high); very_high: lookup(nt, {true: R(standard-high)}, R(complex-low))}, 0)`;
     - `final = max(esc, floor)`;
     - `level = lookup(final, rank → name, default "standard-high")`.
   - Body:
     - `harnesses = lookup(kind, {art: ["codex"]; design|plan: ["claude", "codex"]}, default lookup(risk, {low: lookup(eq(kind, "repair"), {true: filter(definitions.harnesses, h, not(eq(h, "opencode")))}, []); medium: lookup(and(nt, contains(standard_levels, level)), {true: ["claude", "codex", "opencode"]}, ["claude", "codex"])}, ["claude", "codex"]))`. The result is then filtered so that `claude` is removed at a deep level for kinds outside design, review, analysis, plan and spec.
     - `prefer = lookup(kind, {design|plan: ["claude"]}, lookup(and(eq(risk, "low"), nt), {true: ["opencode"]}, []))`.
     - `hold = eq(kind, "art")`.
     - `reasons = map(filter([{code, detail, when: <condition>} …], r, r.when), r, {code: r.code, detail: r.detail})`, for `level_from_kind`, `hint_respected`, `hint_capped`, `escalated_after_failure` and `raised_for_risk`.
     - `classification = {labels, flags, rationale}`.
   - `routed` → `done`; `stale` → `skip`; `refused` → `unservable`.
5. `assume`: Invoke `work.route` with a literal route: `standard-high`, `["claude", "codex"]`, reason `risk_assumed`. Edges as for `route`.
6. Returns: `done`, `skip`, `unservable`, `failed`.

**Size.** 10 steps, about 15 KB of JSON. Acceptance 3 ("a second opinion on high-risk work") adds `ask2`, `await2` and a second `route` step keyed on `[risk1, risk2]`.

**Sentences that cannot be expressed in the playbook:**
1. "If nothing allowed can take the task right now, hold it until something can. That is a wait, not a failure." This is placement's behaviour (spec 0011 §4), not the playbook's.
2. "If nothing allowed could ever take it, leave it unrouted and visible, say why on the task, and raise it for my attention. Never leave a task waiting with no explanation." W6 is in the server by §7. The playbook can only end `unservable`.
3. "Route a task when it becomes ready, and again when it is retried or reopened." When a run starts is the host's emission rule (B5). Triggers have no guard.
4. "Never raise the level because providers are busy, and never use a task's priority to choose anything." This holds because `work.read` exposes neither; it is not a rule the playbook states.
5. "Keep the Deep Claude models for design, review and deep analysis. Don't spend them on escalated bug fixes." A route names harnesses, not models. It holds only when a deep level's Claude candidate is the Deep Claude model.
6. "…An outage, a lost worker or review feedback on finished work do not [count]." This is expressible only through the host's definition of `charged_failures`. The playbook cannot see failure reasons.
7. "Always record on the task what was decided and why…" Reasons are codes with fixed text, because there is no string building. The classification is attached as data.

Ambiguous, where an agent may `ask` (a risk for acceptance 1):
- "Research is Standard, high effort, unless it is clearly harder" names no higher level.
- "at least Complex" names no effort; the reading is the lowest assigned Complex level.

---

## Global Constraints

- **Repositories and worktrees.**
  - Engine tasks run in a second worktree, `/home/jkern/dev/aq-platform/wt/status-sync-playbooks-authoring`, on branch `feat/playbook-authoring` cut from `feat/internal-work`. They are merged back into `feat/internal-work` after the live-state plan's Task 12.
  - Server tasks run on `feat/internal-work` in `/home/jkern/dev/aq-platform/wt/status-sync-server`. Dashboard tasks run on `feat/internal-work` in `/home/jkern/dev/aq-platform/wt/status-sync-dashboard`.
  - Nothing is pushed.
- **Live-state plan.** `docs/superpowers/plans/2026-11-13-live-state-only.md` Tasks 4–12 change `crates/aq-playbooks-{runner,postgres}`, `crates/aq-playbooks-cli/src/{main.rs,simulation.rs}`, CLI tests, workspace `Cargo.toml` pins and `Cargo.lock`. Engine tasks that touch any of these start only after its Task 12 is merged (each task says when).
- **Migrations.** 0119 and 0120 are frozen. New schema means new migrations, in order: 0121 (D1), 0122 (C1), 0123 (C2). Each migration sets `meta.state_format_version`.
- **Line ratchet.** `python3 scripts/build-budget.py` passes after every server task. aq-server lib ≤ 110267 lines. Do not run `--update` before Task F1. New logic goes into new crates (`aq-playbook-authoring`) or existing non-aq-server crates (`aq-playbook-domain`, `aq-internal-work`, `aq-routing`, `aq-supervisor-host`).
- **Test binaries.** The test-binary count stays at 81. New crates use `#[cfg(test)]` unit tests; database tests go in existing `it_*` binaries (`it_playbook`, `it_access`, `it_schema`, `it_profiled`, `it_dashboard`).
- **Every server test run is under `timeout 900`.**
  - Engine definition of done: `cargo fmt --check`; `cargo clippy --locked --workspace --all-targets -- -D warnings`; `cargo test --locked --workspace`.
  - Server definition of done: the test binary of the task, `cargo clippy --locked --workspace --all-targets -- -D warnings` once at the end, `python3 scripts/build-budget.py`, and `python3 scripts/check-playbook-source.py` when `vendor/` changes.
  - Dashboard definition of done: `pnpm typecheck`, `pnpm lint`, `pnpm test`.
- **Authoring bounds.**
  - Run deadline 1 800 000 ms (30 min); per-candidate floor 300 000 ms.
  - Cases: at least 5, at most 40.
  - `judgment.assess`: ≤ 16 fields, ≤ 64 choices each, ≤ 128 B per choice, ≤ 32 flags, request ≤ 65 536 B.
  - Lookup ≤ 256 cases with `when` ≤ 1024 B; `let` ≤ 16 bindings.
  - Transcript ≤ 262 144 B per candidate, items clipped to 2048 B.
  - `out/outcome.json` ≤ 1 048 576 B.
- **Grants (R2):** 24 h expiry, renewed every 12 h.
- **Route emission:** at most once per task version per 120 000 ms. Unrouted for 600 000 ms is a W6 failure.
- **Names** fixed here and used everywhere:
  - source id `routing`; binding (playbook id) `aq.routing`;
  - commands `work.read`, `work.route`, `judgment.assess`; events `aq.work.route_needed`, `aq.judgment.assessed`;
  - capabilities `work.read`, `work.route`, `judgment.assess`;
  - profile `host_v9` / `CatalogProfile::Host9`.
- **Owner-facing text** in the dashboard and in diagnostics never shows step ids, fingerprints or command names when a sentence or situation can be shown instead (spec §2.4, §2.6).

## Review Focus

1. **A task edited while its routing run is in flight.** `work.route` with an older revision returns `stale`, writes no route, and the next emission routes the new version. Test: Task B3, `a_route_for_an_older_task_revision_is_stale_and_writes_nothing`.
2. **The owner saves the routing document twice in quick succession.** The older run is superseded, and its save never activates over the newer one. Test: Task C4, `a_save_from_a_superseded_run_is_recorded_and_never_activated`.
3. **Settings change during an authoring run.** The save is refused as stale facts, the active playbook stays, and authoring starts again. Test: Task C4, `a_save_whose_facts_changed_is_refused_and_authoring_starts_again`.
4. **The agent ends without an outcome, or with a malformed or oversized one.** The run fails `invalid_output`, nothing changes, and the previous playbook keeps routing. Test: Task C2, `an_agent_that_ends_without_an_outcome_fails_the_run_and_changes_nothing`.
5. **The server restarts with an authored artifact active.** `route_needed` still reaches it. Test: Task C4, `after_restart_the_active_authored_artifact_still_receives_its_trigger_event`.

---

## Order and parallelism

```
Engine worktree (feat/playbook-authoring)        Server (feat/internal-work)           Dashboard
A1 ─┐                                            D1 ─► C1 ─► C2 ──────────────┐
A2 ─┤ concurrent with live-state 4–12            (all concurrent with engine) │        E2 (after C2)
A3 ─┤                                                                          │
A4 ─┘                                                                          │
   live-state Task 12 merged ─► A5 ─► A6 ─► C5 ─► A7 ─► B1 ─► B2 ─► B3 ─► B4 ─► B5 ─┤
                                                         └► D4 (after D2)      C3 ─► C4 ─► C6 ─► D2 ─► D3 ─► E1 ─► F1, F2
```

- Concurrent now: A1–A4 (engine second worktree) together with D1, C1, C2 (server).
- B1 waits for A7, because it vendors the engine once, at 0.2.0, with both plans in it.
- E1 can start on mocks once C6's response shapes are merged.
- E2 can start once C2 is merged.

---

## Engine tasks (aq-playbooks)

### Task A1: `lookup` and `let` expressions

Runs concurrently with the live-state plan: model crate only.

**Files:**
- Modify: `crates/aq-playbooks-model/src/lib.rs` (`Expr` :248-306, `eval_inner` :823-1056, `infer_expr` :2315-2494), `crates/aq-playbooks-model/FORMAT.md`
- Test: `crates/aq-playbooks-model/tests/lookup_let.rs` (new)

**Interfaces:**
- Produces (serde `tag = "op"`, `deny_unknown_fields`):

```rust
Expr::Lookup { key: Box<Expr>, cases: Vec<LookupCase>, default: Box<Expr> }   // {"op":"lookup",...}
Expr::Let { bindings: Vec<LetBinding>, body: Box<Expr> }                       // {"op":"let",...}
pub struct LookupCase { pub when: Value, pub then: Expr }
pub struct LetBinding { pub name: String, pub value: Expr }
pub const MAX_LOOKUP_CASES: usize = 256;
pub const MAX_LOOKUP_KEY_BYTES: usize = 1024;
pub const MAX_LET_BINDINGS: usize = 16;
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn lookup_evaluates_only_the_matching_case()          // key "bugfix" -> "standard-high"; a non-matching `then` refs an absent root and the result is still Known
#[test] fn lookup_default_on_no_match_and_unresolved_key_propagates() // Known "zzz" -> default; Unknown key -> Unknown; Missing key -> Missing
#[test] fn lookup_result_type_is_the_union_of_its_results()  // infer == String{values:{"complex-high","simple-high","standard-high"}}
#[test] fn lookup_refuses_duplicate_incompatible_or_too_many_cases() // dup canonical `when`; `when: 1` vs string key; 257 cases; 1025-byte key
#[test] fn lookup_keys_may_be_arrays_and_integers()          // key ["high", true]; key integer rank 7 -> "complex-high"
#[test] fn let_binds_in_order_and_once()                      // b may ref a; body sees both
#[test] fn let_refuses_reserved_and_shadowing_names()         // "input","event","definitions","vars", an existing root, 17 bindings
#[test] fn max_of_two_ranks_without_arithmetic()              // let a=5,b=7: lookup(lt(a,b),[{when:true,then:b}],a) == 7, and == 7 for a=7,b=5
#[test] fn an_invoke_input_using_lookup_and_let_validates_against_an_enum_target() // validate_structure() is empty; Artifact::seal().verify() ok
```

- [ ] **Step 2: Run to verify they fail.** `cargo test --locked -p aq-playbooks-model --test lookup_let`. Expected: compile errors (`Expr::Lookup` unknown).
- [ ] **Step 3: Implement** the variants in `eval_inner` and `infer_expr`, as Spec check item 1 describes. A lookup charges `cases.len()` to the element budget. `let` inserts lexical roots like `Map.item`. Document both in FORMAT.md, including that `FORMAT_VERSION` stays 1 and that an older runner refuses an artifact using them when it deserializes it.
- [ ] **Step 4: Run to verify they pass**, then `cargo test --locked -p aq-playbooks-model -p aq-playbooks-compiler`.
- [ ] **Step 5: Commit** `feat(model): lookup and let expressions`

### Task A2: Reading an asynchronous answer through a named join

Runs concurrently: model crate only.

**Files:**
- Modify: `crates/aq-playbooks-model/src/lib.rs` (`member_outcomes` :2955-2979: the Invoke branch accepts only `path.is_empty()`), `crates/aq-playbooks-model/FORMAT.md` (join section)
- Test: `crates/aq-playbooks-model/tests/async_answer.rs` (new)

**Interfaces:**
- Consumes: 2.1.0 `judgment.request` from `contracts/aq-host-2.1.0/registry.json`, loaded in the test with `include_str!`.
- Produces: the validation rule "a join member that is an Invoke handle is `{op: ref, root: <bind>}` with no path". The runtime proof comes in Task A5.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn join_over_an_invoke_handle_narrows_completed_to_the_accepted_outcome() // ref answer.c.output.choice types as String<=512 on `completed`
#[test] fn the_handle_path_member_form_is_refused_by_validation()                // member ref j path ["handle"] -> issue code "join_member_type"
#[test] fn no_match_and_timeout_edges_do_not_see_a_narrowed_answer()             // ref answer.c.output.choice on the no_match target -> "expression_type"
```

- [ ] **Step 2: Run to verify they fail.** The second test fails, because `["handle"]` is accepted today.
- [ ] **Step 3: Implement** the restriction. Update FORMAT.md's join paragraph with the ask → join → read pattern.
- [ ] **Step 4: Run to verify they pass**, plus `cargo test --locked --workspace`.
- [ ] **Step 5: Commit** `fix(model): a join reads an operation handle by its binding, as the runner does`

### Task A3: Draft workspace with names in place of identities

Runs concurrently: compiler crate only.

**Files:**
- Create: `crates/aq-playbooks-compiler/src/draft.rs`
- Modify: `crates/aq-playbooks-compiler/src/lib.rs` (remove `Proposal.questions`, `unsupported_features`, `Severity::Question` and the `diff` alias; `validate` fills `Diagnostic.line` from `source_map` as today), `crates/aq-playbooks-cli/src/pilots.rs:864-884` (`compile_input` builds a `Proposal` without the removed fields; one line)
- Test: unit tests in `draft.rs`; `crates/aq-playbooks-compiler/tests/draft.rs` (new)

**Interfaces:**
- Produces:

```rust
pub struct SourceRecord { pub id: String, pub binding: String, pub scope: String, pub purpose: String,
    pub commands: BTreeSet<String>, pub events: BTreeSet<String>, pub capabilities: BTreeSet<String>,
    pub facts_version: String }                                   // record.json; deny_unknown_fields
pub struct Workspace { pub markdown: String, pub record: SourceRecord, pub catalog: Registry,
    pub facts: BTreeMap<String, Value>, pub draft: Value }
impl Workspace { pub fn read(dir: &Path) -> Result<Self, Vec<Diagnostic>>; }  // source.md, record.json, catalog.json, facts.json, draft.json
pub fn source_text(record: &SourceRecord, entries: &BTreeSet<String>, facts: &BTreeSet<String>, markdown: &str) -> (String, u32 /*header lines*/);
pub fn retain_registry(catalog: &Registry, commands: &BTreeSet<String>, events: &BTreeSet<String>)
    -> Result<(Registry, BTreeSet<String> /*capabilities*/), String>;
pub fn fill(ws: &Workspace) -> Result<CompileInput, Vec<Diagnostic>>;
pub fn check(ws: &Workspace) -> Vec<Diagnostic>;                // fill + validate
pub fn seal(ws: &Workspace) -> Result<Artifact, Vec<Diagnostic>>;
pub fn unfill(artifact: &Artifact) -> Value;                    // artifact -> draft.json (worked examples, previous playbook)
pub struct Comparison { pub change: ChangeKind, pub abilities_changed: bool, pub diff: SemanticDiff }
pub fn compare(previous: &Artifact, next: &Artifact) -> Result<Comparison, ModelError>;
```

- `retain_registry` adds each async command's `reconciliation_command` and `completion_event`, as `coding.rs:431-449` does. It refuses a name absent from `catalog`.
- `fill` rewrites only `steps.*.op.command` (invoke), `steps.*.op.event` (wait, when not null) and `triggers[*].event`. It refuses a draft carrying `id`, `scope` or `required_capabilities` that differ from the record. It defaults `limits`, shifts `source_map` lines by the header lines, and captures each fact as `CapturedDefinition {version: facts_version, digest, value}`.
- `abilities_changed` is true when capabilities were added or `changed_commands`/`changed_events` is non-empty.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_draft_with_names_fills_to_the_same_artifact_as_a_hand_built_proposal() // digest equal to a CompileInput built with fingerprints
#[test] fn an_unknown_command_names_the_allowed_ones()       // message contains "work.reed" and "work.read, work.route"
#[test] fn an_async_command_brings_its_reconciliation_command_and_completion_event()
#[test] fn the_header_is_written_by_the_tool_and_lines_shift() // a diagnostic on a step mapped to source.md line 12 reports line 12 + header lines
#[test] fn unknown_fields_are_still_refused_with_a_draft_position() // "stepz" at top level -> diagnostic with draft.json line/column
#[test] fn a_draft_claiming_another_scope_or_capabilities_is_refused()
#[test] fn compare_names_wording_only_behaviour_and_abilities() // Presentation, Executable, abilities_changed on a new command
#[test] fn unfill_then_fill_round_trips()
```

- [ ] **Step 2: Run to verify they fail.** `cargo test --locked -p aq-playbooks-compiler --test draft`.
- [ ] **Step 3: Implement `draft.rs`** with the signatures above, and remove the three proposal fields and `Severity::Question`. Update the existing compiler unit tests that built them.
- [ ] **Step 4: Run to verify they pass**, plus `cargo test --locked --workspace`.
- [ ] **Step 5: Commit** `feat(compiler): drafts with contract names, filled by the tool`

### Task A4: Host catalog 2.2.0

Runs concurrently: new contract directory, `catalog.rs`, and a new test file.

**Files:**
- Create: `contracts/aq-host-2.2.0/{registry.json,manifest.json,fixtures.json,README.md}`, `crates/aq-playbooks-cli/tests/catalog_v2_2_contract.rs`
- Modify: `crates/aq-playbooks-cli/src/catalog.rs` (`VERSION_2_2`, `REGISTRY_2_2_JSON`, `pub fn catalog_v2_2() -> Registry`)

**Interfaces:**
- Produces: `catalog_v2_2()`, which holds 2.1.0's 12 commands and 9 events byte-identical, plus the following. All new contracts have `scopes ["project"]`, producers `["aq.host.bridge@1"]`, `max_payload_bytes 65536`, and event retention 86 400 000.
  - **`work.read` 1.0.0.** Capability `work.read`; observation; recovery observation; deadline 10 000.
    - Input: `{subject}`.
    - `observed`: `{reference, subject, facts: {kind ≤64, title ≤512, description ≤8192, origin ∈ {owner, agent, supervisor, playbook, unknown}, level_hint ≤64, charged_failures 0..1000, ended_attempts 0..1000, route ∈ {none, router, owner}}}`.
    - `refused`: `{reference, subject, reason ∈ {unknown_task, not_on_router}}`.
    - `public_result_fields`: `[reference, subject]`.
  - **`work.route` 1.0.0.** Capability `work.route`; mutation; keyed_idempotent; deadline 10 000.
    - Input: `{subject, level ≤64, harnesses, providers, prefer: Array{String ≤64, max 16}, hold: Boolean, reasons: Array{{code ≤64, detail ≤400}, max 16}, classification: Nullable{{labels: Array{String ≤128, 16}, flags: Array{String ≤64, 32}, rationale ≤4096}}}`.
    - Outcomes: `routed`, `stale` (both `{reference, subject}`) and `refused {reference, subject, reason ∈ {not_offered, owner_route, not_on_router, unknown_task}}`.
  - **`judgment.assess` 1.0.0.** Capability `judgment.assess`; mutation; keyed_idempotent; async, with completion event `aq.judgment.assessed`, correlation `operation`, reconciliation `operation.get`.
    - Input: `{subject, question ≤4096, context: {items: Array{{label ≤128, text ≤8192}, 16}}, fields: Array{{id ≤64, description ≤1024, choices: Array{String ≤128, 64}}, 16}, flags: Array{{id ≤64, description ≤1024}, 32}, rationale_max_bytes 0..4096, budget: {deadline_ms 1000..600000, max_output_tokens 1..65536}}`.
    - Outcomes:
      - `accepted {handle}`;
      - `assessed {reference, subject, labels: Array{String ≤128, 16}, flags: Array{String ≤64, 32}, rationale ≤4096, provenance}` (provenance as in 2.1.0 `judgment.request`);
      - `failed {reference, subject, reason ∈ {indeterminate, provider_error, provider_unavailable, deadline_exceeded}}`;
      - `refused {reference, subject, reason ∈ {invalid_request, budget_exceeded}}`.
  - **Event `aq.judgment.assessed` 1.0.0.** Payload `{outcome ∈ {assessed, failed, refused}, output: Union}`; correlation `[operation]`; subject field `output.subject.id`; revision field `output.subject.revision`.
  - **Event `aq.work.route_needed` 1.0.0.** Payload `{subject, reason ∈ {ready, retried, reopened, repeat}}`; correlation `[]`; subject field `subject.id`; revision field `subject.revision`.
- The manifest gains `commands_added` and `events_added`, and `supersedes` 2.1.0. The fingerprint is `digest(("2.2.0", &registry))`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn catalog_v2_2_manifest_and_fixtures_are_pinned()
#[test] fn catalog_v2_2_keeps_every_2_1_0_contract_byte_identical()  // all 12 + 9 fingerprints equal compatibility.unchanged_*
#[test] fn an_artifact_sealed_on_2_1_0_validates_against_the_2_2_0_registry()
#[test] fn judgment_assess_completion_is_assignable_to_its_event()   // ArtifactContent::validate_structure has no async_contract issue
#[test] fn valid_and_invalid_fixture_cases_validate_as_declared()
```

- [ ] **Step 2: Run to verify they fail.** `cargo test --locked -p aq-playbooks-cli --test catalog_v2_2_contract`.
- [ ] **Step 3: Write the registry** with a small generator test (`#[ignore] fn write_catalog_v2_2()`) built from the `catalog.rs` helpers (`object`, `enumeration`, `list`). Commit the generated JSON, and write the README in the 2.1.0 layout.
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(catalog): host catalog 2.2.0 — task facts, routes, structured judgments, route_needed`

### Task A5: Simulation library, cases and the authoring CLI

Starts after live-state Task 12 is merged into this branch (it touches `cli/src/{main.rs,simulation.rs}`).

**Files:**
- Create: `crates/aq-playbooks-simulation/{Cargo.toml,src/lib.rs,src/cases.rs}` (version `0.2.0`, workspace member), `crates/aq-playbooks-cli/src/authoring.rs`
- Move: `crates/aq-playbooks-cli/src/simulation.rs` → `crates/aq-playbooks-simulation/src/lib.rs`. The CLI depends on the new crate, and `lib.rs` drops `pub mod simulation`.
- Modify: `crates/aq-playbooks-cli/src/main.rs` (add `Check`, `Seal`, `Try`, `Compare`, `Facts`, `Save`, `Ask`, `Unable`; remove the `CompileInput` `Validate`/`Compile` and `Diff`; `Commands`/`Events` default to `./catalog.json` when present), root `Cargo.toml`
- Test: unit tests in `cases.rs`; `crates/aq-playbooks-cli/tests/authoring_cli.rs` (new)

**Interfaces:**
- Consumes: `draft::{Workspace, seal, check, compare}` (A3), `catalog_v2_2()` (A4).
- Produces:

```rust
pub struct Case { pub name: String, pub situation: String, pub start: CaseStart,
    pub returns: BTreeMap<String, Vec<CaseReturn>>, pub expect: Expectation }
#[serde(tag = "kind")] pub enum CaseStart { Event { event: String, payload: Value }, Entry { entry: String, input: Value } }
#[serde(untagged)] pub enum CaseReturn { Answer { outcome: String, output: Value }, Pending { pending: bool } }
pub struct Expectation { pub outcome: String, #[serde(default)] pub calls: Vec<ExpectedCall> }
pub struct ExpectedCall { pub command: String, pub input: Value }   // object subset match; arrays and scalars exact
pub struct Call { pub step: String, pub command: String, pub input: Value, pub outcome: Option<String> }
pub struct CaseReport { pub name: String, pub situation: String, pub passed: bool,
    pub outcome: Option<String>, pub calls: Vec<Call>, pub problems: Vec<String> }
pub const MIN_CASES: usize = 5;  pub const MAX_CASES: usize = 40;
pub fn run_case(artifact: &Artifact, case: &Case) -> anyhow::Result<CaseReport>;
pub fn run_cases(artifact: &Artifact, cases: &[Case]) -> anyhow::Result<Vec<CaseReport>>; // refuses <5 or >40
pub struct Outcome { kind: OutcomeKind, ... }  // out/outcome.json, below
```

- Rules for a case:
  - An async command's `Answer` is returned as `Accepted {handle: "case-<n>"}`, then its completion event is admitted. `Pending` admits and never completes; the clock moves to the step deadline.
  - A call with no return left becomes the problem `"step <s> called <command> but the case gives no return for it"`, and the case fails.
  - The run's outcome is read through storage inspection (after spec 0003).
  - Event envelopes are built from the contract: first trusted producer, `case-<i>` ids, the playbook scope, subject and revision from the contract's subject/revision fields.
- CLI, with the workspace = cwd or `--workspace DIR`; every command prints JSON:
  - `check` exits 1 on diagnostics.
  - `try CASE.json` prints the `CaseReport`.
  - `compare` prints the `Comparison`, or `{"previous": null}`.
  - `save` runs `check`, `seal`, `run_cases(cases/*.json)` and the capability test `required_capabilities ⊆ record.capabilities`. On success it writes `out/outcome.json = {"kind":"saved","draft":…,"cases":[…],"artifact":"sha256:…","facts_digest":"sha256:…"}`. On refusal it prints the reasons and writes nothing.
  - `ask --line N "question"` writes `{"kind":"question","question":…,"line":N,"sentence":<line text>}`.
  - `unable --line N "reason"` writes `{"kind":"unable",…}`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_case_records_each_call_with_its_input()
#[test] fn an_async_answer_reaches_the_playbook_through_the_join()  // the A2 runtime proof: route input level comes from labels[0]
#[test] fn a_pending_answer_takes_the_timeout_path()
#[test] fn a_call_without_a_return_fails_the_case_and_names_the_step()
#[test] fn an_expected_call_matches_by_subset_and_order()
#[test] fn a_triggered_case_starts_from_the_event()
#[test] fn fewer_than_five_or_more_than_forty_cases_are_refused()
// authoring_cli.rs (runs the binary in a temp workspace with the 2.2.0 catalog):
#[test] fn check_try_compare_save_on_a_workspace()
#[test] fn save_refuses_a_failing_case_and_writes_no_outcome()
#[test] fn ask_and_unable_write_the_outcome_with_the_sentence()
#[test] fn commands_list_defaults_to_the_workspace_catalog()
```

- [ ] **Step 2: Run to verify they fail.** `cargo test --locked -p aq-playbooks-simulation -p aq-playbooks-cli`.
- [ ] **Step 3: Implement**, moving `simulation.rs` unchanged first (its own commit inside the task), then add `cases.rs` and `authoring.rs`.
- [ ] **Step 4: Run to verify they pass**, plus `cargo test --locked --workspace`.
- [ ] **Step 5: Commit** `feat(simulation): cases, and the agent's check/try/compare/save tools`

### Task A6: Retire CLI leftovers; worked example workspaces

After A5.

**Files:**
- Move: `crates/aq-playbooks-cli/src/{fake_host.rs,pilots.rs}` → `crates/aq-playbooks-cli/tests/support/{fake_host.rs,pilots.rs}`, with the pilot `routing()` (`pilots.rs:613-700`) deleted. Tests use `#[path]` modules; `src/lib.rs` drops both modules.
- Modify: `crates/aq-playbooks-cli/src/main.rs` (remove `Surface`, `Interfaces`, `Simulate`; `Examples --output DIR` writes the worked workspaces), `crates/aq-playbooks-cli/tests/{host_pilots.rs,catalog_contract.rs,catalog_v2_contract.rs,catalog_v2_1_contract.rs,cli.rs}`
- Create: `docs/authoring/examples/escalate-on-judgment/{source.md,record.json,catalog.json,facts.json,draft.json,cases/*.json}` and `docs/authoring/examples/notify-when-ready/…` (2.2.0 catalog, each ≤ 8 steps, ≥ 5 cases, neither about routing)
- Test: `crates/aq-playbooks-cli/tests/authoring_examples.rs` (new)

**Interfaces:**
- Produces: `aq_playbooks_compiler::docs::example_files() -> Vec<(&'static str, &'static str)>` (`include_str!` of both workspaces). Task C5 adds the guide constants to the same module.

- [ ] **Step 1: Write the failing test**: `fn every_worked_example_checks_saves_and_its_cases_pass()` (runs `check` and `save` on each).
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Write the two workspaces** and make the moves.
- [ ] **Step 4: Run** `cargo test --locked --workspace`. Expected: PASS, and `grep -rn "pub mod fake_host\|pub mod pilots\|Command::Surface\|Command::Interfaces\|Command::Simulate" crates/*/src` prints nothing.
- [ ] **Step 5: Commit** `refactor(cli): test-only fakes leave the CLI; worked authoring examples`

### Task C5: Authoring guide and language reference (engine)

After A6 (it embeds the examples). Drafting can start earlier.

**Files:**
- Modify: `skills/aq-playbook-author/SKILL.md` (becomes the guide; frontmatter kept)
- Create: `crates/aq-playbooks-compiler/LANGUAGE.md`, `crates/aq-playbooks-compiler/src/docs.rs` (`pub const GUIDE: &str = include_str!("../../../skills/aq-playbook-author/SKILL.md"); pub const LANGUAGE: &str = include_str!("../LANGUAGE.md"); pub const FORMAT: &str = include_str!("../../aq-playbooks-model/FORMAT.md");`)
- Test: `crates/aq-playbooks-compiler/tests/docs.rs` (new)

**Interfaces:**
- Produces: `docs::{GUIDE, LANGUAGE, FORMAT, example_files}`, which the server writes into every authoring workspace (C3).

- [ ] **Step 1: Write the failing tests**:
  - `every_json_block_in_language_md_parses_as_an_expr_or_step` (fenced blocks tagged `json expr` or `json step`);
  - `every_validation_code_has_an_entry` (each `issue(…, "<code>", …)` string in model `lib.rs` appears as a `### <code>` heading);
  - `the_guide_names_every_tool` (`check`, `try`, `compare`, `facts`, `save`, `ask`, `unable`, `commands`, `events`).
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Write both documents** with the contents listed in Spec check item 4. The guide's rules:
  - read `source.md`, `record.json` and `facts.json` first;
  - every sentence that changes the process becomes a step or a case;
  - cover each situation the document distinguishes, with ≥ 5 cases;
  - `ask` only when the document cannot be followed without guessing, and in the owner's words, never naming commands or steps;
  - `unable` when the catalog cannot serve a sentence;
  - finish with exactly one of `save`, `ask`, `unable`.
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `docs(authoring): the guide and language reference an agent works from`

### Task A7: Engine 0.2.0 for the server

After A1–A6, C5 and live-state Task 12.

**Files:**
- Modify: `crates/aq-playbooks-{model,compiler}/Cargo.toml` (`version = "0.2.0"`), root `Cargo.toml` (workspace pins `=0.2.0` for model, compiler and simulation), `Cargo.lock`
- Create: `docs/implementation/0004-playbook-authoring.md` (what was added, the API differences listed for the server, and the guide location)

- [ ] **Step 1: Bump** the versions. Then run `cargo update -p aq-playbooks-model -p aq-playbooks-compiler -p aq-playbooks-simulation --offline`.
- [ ] **Step 2: Run** `cargo fmt --check && cargo clippy --locked --workspace --all-targets -- -D warnings && cargo test --locked --workspace` and `crates/aq-playbooks-postgres/scripts/test-postgres.sh`. Expected: all pass.
- [ ] **Step 3: Merge** `feat/playbook-authoring` into `feat/internal-work` (no fast-forward), and record the merge revision in the implementation note.
- [ ] **Step 4: Commit** `chore(release): engine 0.2.0 with authoring tools`

---

## Server tasks (aq-server)

### Task D1: Retire the decision table

Runs now, in parallel with the engine. It frees about 430 aq-server lib lines.

**Files:**
- Delete: `crates/aq-routing/src/{policy.rs,plan.rs,compile.rs,fixtures.rs}`, `crates/aq-server/src/routing/compile.rs`, `crates/aq-server/tests/it_profiled/profile_catalog/routing_compile.rs`
- Modify:
  - `crates/aq-routing/src/lib.rs` (modules; `pub use plan::…` :10) and `classify.rs` (receives `offers` from `compile.rs:341-356`); `crates/aq-routing/Cargo.toml` (drop feature `fixtures`);
  - `crates/aq-server/src/routing.rs` (gains `pub async fn options(state: &AppState) -> Result<Options, AppError>` from `compile.rs:33-54`; `set_enabled(true)` returns `Conflict("Routing can be turned on once a routing playbook is active")`);
  - `crates/aq-server/src/routing/http.rs` (view returns `{"enabled": bool, "active": null, "latest": null}`, the shape the dashboard reads today);
  - `crates/aq-server/src/profile_catalog/settings.rs:60-70` (no compile call), `crates/aq-server/src/lib.rs:513` (no compile serve loop);
  - `crates/aq-server/Cargo.toml:106` (dev-dependency without `fixtures`);
  - `tests/it_profiled/profile_catalog.rs:95`, `tests/it_dashboard/supervisor_turns/admin.rs`, `tests/it_access/routing.rs` (switch tests).
- Create: `crates/aq-server/migrations/0121_retire_routing_policies.sql` (`DROP TABLE routing_policies; ALTER TABLE routing_state DROP COLUMN active_policy_digest;`, meta `121`)
- Test: `tests/it_access/routing.rs`, `tests/it_schema` (migration catalog)

**Interfaces:**
- Produces: `crate::routing::options(&AppState) -> Result<aq_routing::classify::Options, AppError>`, `aq_routing::classify::offers(...)`.

- [ ] **Step 1: Write the failing test**: `routing::routing_cannot_be_switched_on_without_a_routing_playbook` (POST `{enabled: true}` → 409, view `active: null`).
- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^routing::/)'`. Expected: FAIL (today an active policy row allows it).
- [ ] **Step 3: Delete and move** as listed, and add migration 0121.
- [ ] **Step 4: Run** the routing tests, `timeout 900 cargo nextest run -p aq-server --test it_schema`, `-p aq-routing`, and `python3 scripts/build-budget.py`. Expected: PASS and a lower aq-server line count. Do not `--update`.
- [ ] **Step 5: Commit** `refactor(routing): remove the decision table and its compile-on-save`

### Task C1: Sources and authoring runs bookkeeping

After D1. It needs no engine change.

**Files:**
- Create: `crates/aq-playbook-authoring/{Cargo.toml,src/lib.rs,src/source.rs,src/store.rs,src/runs.rs}` (workspace member; depends on `aq-internal-work`, `sqlx`, `serde_json`, `uuid`, `tokio`, `tracing`), `crates/aq-server/migrations/0122_playbook_authoring.sql`
- Modify: root `Cargo.toml` (member)
- Test: unit tests in `source.rs`; `crates/aq-server/tests/it_playbook/playbook_authoring.rs` (new module in `it_playbook/main.rs`)

**Interfaces:**
- Tables (0122):
  - `playbook_sources(id TEXT PK, kind TEXT CHECK IN ('builtin','registered'), binding TEXT UNIQUE, scope TEXT, purpose TEXT, judgment_purpose TEXT, commands JSONB, events JSONB, capabilities JSONB, facts JSONB, markdown TEXT NULL, revision BIGINT, updated_at_ms BIGINT)`;
  - `playbook_authoring_runs(id BIGINT IDENTITY PK, source_id TEXT REFERENCES playbook_sources, source_revision BIGINT, source_digest TEXT, facts_digest TEXT, internal_task_id UUID UNIQUE NULL, status TEXT CHECK IN ('running','saved','question','unable','refused','failed','superseded'), artifact_digest TEXT, artifact JSONB, previous_digest TEXT, comparison JSONB, cases JSONB, question JSONB, unable JSONB, refusal TEXT, created_at_ms BIGINT, finished_at_ms BIGINT)`, with a unique partial index on `(source_id) WHERE status='saved' AND active`, and a column `active BOOLEAN DEFAULT FALSE`;
  - `playbook_source_grants(source_id TEXT, grant_id UUID PK, revision BIGINT, expires_at_ms BIGINT, activated_at_ms BIGINT NULL)`.
- Produces:

```rust
pub struct SourceRecord { pub id: String, pub kind: SourceKind, pub binding: String, pub scope: String,
    pub purpose: String, pub judgment_purpose: aq_internal_work::Purpose, pub commands: BTreeSet<String>,
    pub events: BTreeSet<String>, pub capabilities: BTreeSet<String>, pub facts: Vec<String /*provider name*/>,
    pub revision: u64 }
pub trait Host: AsRef<PgPool> + Clone + Send + Sync + 'static {
    async fn markdown(&self, source: &SourceRecord) -> Result<(String, u64)>;   // routing: the structure document
    async fn facts(&self, source: &SourceRecord) -> Result<BTreeMap<String, Value>>;
}
pub async fn ensure_builtin(pool: &PgPool, record: &SourceRecord) -> Result<()>;
pub async fn register(pool: &PgPool, record: &SourceRecord, markdown: &str) -> Result<()>;
pub async fn requested<H: Host>(host: &H, source: &str) -> Result<Option<i64 /*run id*/>>; // skipped when the source and facts digests equal the active run's
pub async fn supersede_older(pool: &PgPool, source: &str, run: i64) -> Result<u64>;
pub async fn active(pool: &PgPool, source: &str) -> Result<Option<ActiveRun>>;
pub async fn run_for_artifact(pool: &PgPool, digest: &str) -> Result<Option<(String /*source*/, Purpose)>>;
```

- [ ] **Step 1: Write the failing tests** (in `it_playbook::playbook_authoring`):
  - `a_save_of_unchanged_text_and_facts_starts_no_run`;
  - `a_newer_request_supersedes_a_running_one`;
  - `a_builtin_record_is_upserted_once`;
  - `a_registered_source_rejects_a_binding_already_used`;
  - `run_for_artifact_finds_the_source_and_purpose`.
- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^playbook_authoring::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** The patterns of the deleted `routing/compile.rs` (record before submit, supersede, resume after `GRACE_MS = 30_000`) are rebuilt here generically. Submitting the internal task comes in C3.
- [ ] **Step 4: Run to verify they pass**, plus `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(authoring): playbook sources and authoring runs`

### Task C2: Internal work agent mode

After D1. It does not need the vendored engine (tests use a fake native).

**Files:**
- Modify:
  - `crates/aq-internal-work/src/lib.rs` (`Request.agent`; `AGENT_DEADLINE_MS = 1_000..=1_800_000`; `agent_candidate_budget_ms`), `src/store.rs` (claim policy, transcripts), `src/validate.rs` (`agent_prompt`);
  - `crates/aq-supervisor-host/src/internal.rs` (`serve`: at most one agent run; the Codex agent claim rule), `src/internal/claude.rs` and `src/internal/codex.rs` (agent arguments);
  - `crates/aq-internal-work/src/http.rs` (the run read includes the transcript).
- Create: `crates/aq-supervisor-host/src/internal/agent.rs` (materialize files, read the outcome, parse the transcript), `crates/aq-server/migrations/0123_internal_agent_runs.sql` (`workspace_kind` CHECK `IN ('none','agent')` via DROP/ADD CONSTRAINT; `internal_task_transcripts(task_id UUID, candidate_index INT, events JSONB, bytes INT, PRIMARY KEY(task_id, candidate_index))`; meta `123`)
- Test: `crates/aq-server/tests/it_access/internal_work.rs` (fake natives as today), unit tests in `aq-internal-work`

**Interfaces:**
- Produces:

```rust
#[serde(default)] pub agent: Option<AgentWork>          // on Request
pub struct AgentWork { pub files: BTreeMap<String, String>, pub max_turns: u32 /*1..=400*/ }
pub const AGENT_OUTCOME: &str = "out/outcome.json";
pub const AGENT_FALLBACK_RESERVE_MS: i64 = 300_000;
pub fn agent_candidate_budget_ms(deadline_at_ms: i64, claimed_at_ms: i64, candidate_index: u32, candidate_count: usize) -> i64;
pub async fn store::save_transcript(pool: &PgPool, task: Uuid, candidate: u32, events: &Value) -> Result<()>;
```

- Paths in `files` are relative, have no `..`, at most 256 files, at most 1 MiB in total, as Request validation checks.
- The agent run uses the same `validate::answer` on the outcome file's text.
- Claude agent argv:
  - `-p --output-format stream-json --verbose --input-format text --model M --setting-sources user --settings S --strict-mcp-config --no-session-persistence --disable-slash-commands`;
  - `--tools Bash,Read,Write,Edit,Glob,Grep --allowedTools "Bash(aq-playbooks:*)" Read Write Edit Glob Grep --disallowedTools WebFetch WebSearch --max-turns N`.
  - Verify the exact flag spellings against `claude --help` of the pinned native first, and update this list if they differ.
- Codex agent argv: `exec_arguments("workspace-write", …)` plus `--config sandbox_workspace_write.network_access=false`.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test] async fn an_agent_run_gets_its_files_in_the_scratch_directory_and_its_outcome_is_the_answer()
#[tokio::test] async fn an_agent_that_ends_without_an_outcome_fails_the_run_and_changes_nothing()   // Review Focus 4; also malformed and > max_output_bytes
#[tokio::test] async fn the_claude_agent_argv_allows_only_the_engine_cli_in_bash()                // fake native records argv
#[tokio::test] async fn the_codex_agent_runs_in_a_workspace_write_sandbox_without_network()
#[tokio::test] async fn the_agent_environment_has_no_database_url_or_server_credential()
#[tokio::test] async fn only_one_agent_run_holds_a_slot_and_answer_runs_keep_the_other()
#[tokio::test] async fn a_codex_agent_run_waits_while_a_blocking_codex_only_task_is_queued()
#[tokio::test] async fn the_transcript_is_kept_clipped_and_redacted()
#[test] fn agent_budget_keeps_five_minutes_for_each_later_candidate()  // (1_800_000 left, 0 of 2) -> 1_500_000; (400_000, 0 of 2) -> 300_000
```

- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^internal_work::/)'`. Expected: FAIL.
- [ ] **Step 3: Implement.** The agent prompt is the request's instructions followed by "Your workspace is the current directory; finish with `aq-playbooks save`, `ask` or `unable`."
- [ ] **Step 4: Run to verify they pass**, plus `-p aq-internal-work -p aq-supervisor-host` and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(internal-work): agent mode — a harness with tools in a workspace, bounded by time`

### Task B1: Re-vendor the engine at 0.2.0

After A7. This is the live-state plan's "aq-server follow-on" plus this plan's crates, in one task.

**Files:**
- Create: `vendor/aq-playbooks-<rev>/` (`git archive --format=tar <rev> Cargo.toml Cargo.lock crates scripts docs/implementation/current-catalog-inspection-design.md skills/aq-playbook-author contracts/aq-host-2.2.0`), `vendor/aq-playbooks-source-0.2.0.json`, `vendor/aq-playbooks-source-0.2.0.tar`
- Modify:
  - `crates/aq-playbook-host/Cargo.toml:8-10` (`=0.2.0`, new path), `crates/aq-server/Cargo.toml:104` (compiler `=0.2.0`), root `Cargo.toml` `exclude`, `crates/aq-playbook-host/README.md` (version section);
  - the follow-on call sites: `crates/aq-playbook-host/src/lib.rs:125-137,363-367,629-661`; `crates/aq-server/src/playbooks/driver.rs:515-560,544,709,958`; `artifacts/tests.rs:124`; `coding/tests.rs:261,465`; `crates/aq-playbook-host/tests/{owner.rs:123,catalog.rs:22,130}`.
- Test: `it_playbook` (whole binary), `-p aq-playbook-host`

**Interfaces:**
- Produces: `aq_playbooks_compiler::{draft, docs}` and `aq_playbooks_simulation::{Case, run_cases, MIN_CASES, MAX_CASES}`, available to server crates at `=0.2.0`. The engine follow-on items 1–8 are applied as that section prescribes. Item 9 is in F1.

- [ ] **Step 1: Write the failing test**: `playbook_retention::the_owner_applies_ten_minute_event_and_admission_windows` (reads the engine's `RetentionPolicy` after start: `event_replay_ms 600_000`, `admission_replay_ms 600_000`).
- [ ] **Step 2: Run to verify it fails.**
- [ ] **Step 3: Vendor and wire** each follow-on item. Map `STALE_REQUEST` to the existing hold/`Unknown` path, never to a fresh admission.
- [ ] **Step 4: Run** `timeout 900 cargo nextest run -p aq-server --test it_playbook`, `timeout 900 cargo nextest run -p aq-playbook-host`, `python3 scripts/check-playbook-source.py` and `python3 scripts/build-budget.py`. Expected: PASS.
- [ ] **Step 5: Commit** `build(playbooks): vendor aq-playbooks 0.2.0 (live state, authoring tools)`

### Task B2: Adopt catalog 2.2.0 as Host9

After B1.

**Files:**
- Create: `vendor/aq-host-catalog-2.2.0/` (copy of the engine's `contracts/aq-host-2.2.0`), `vendor/aq-host-catalog-2.2.0-source.json`
- Modify: `crates/aq-server/src/playbooks/review_domain.rs` (`Host9`, `HOST9_VERSION`, `HOST9_FINGERPRINT`, `host9_registry()`, `number()` 9, `is_host2`, `validate_scope` with `HOST9_CAPABILITIES`), `crates/aq-server/src/playbooks/config.rs` (`host_v9`, `installation_routing: bool` valid only with `host_v9`; R1)
- Test: `tests/it_playbook/playbook_host9.rs` (new module)

**Interfaces:**
- Produces: `CatalogProfile::Host9`, `pub const HOST9_CAPABILITIES: [&str; 15]` (`HOST7_CAPABILITIES` + `work.read`, `work.route`, `judgment.assess`), and `Loaded.installation_routing`.

- [ ] **Step 1: Write the failing tests**:
  - `host9_registry_matches_the_pinned_manifest`;
  - `a_host8_coding_artifact_installs_in_a_host9_namespace`;
  - `host9_scope_admits_the_new_capabilities_and_nothing_else`;
  - `installation_routing_is_refused_without_host_v9`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the module and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(playbooks): Host9 profile on host catalog 2.2.0`

### Task B3: `work.read` and `work.route`

After B2.

**Files:**
- Create: `crates/aq-playbook-domain/src/host9.rs` (pure)
- Modify: `crates/aq-playbook-domain/Cargo.toml` (`aq-routing`), `crates/aq-routing/src/lib.rs` (`CompactRoute` gains `#[serde(default)] a: u64`, the ended attempts when routed; `Route.ended_attempts`), `crates/aq-server/src/playbooks/host_adapters.rs` (dispatch the two commands; the R1 cross-project check)
- Test: unit tests in `host9.rs`; `tests/it_playbook/playbook_host9.rs`

**Interfaces:**
- Consumes: `crate::routing::{write, options}`, the activation's artifact digest (`driver.binding_artifact`), and `playbook_authoring_runs` (source revision).
- Produces:

```rust
pub struct WorkFacts { pub kind: String, pub title: String, pub description: String, pub origin: Origin,
    pub level_hint: String, pub charged_failures: u32, pub ended_attempts: u32, pub route: RouteSource }
pub fn read_output(task: Uuid, version: i64, facts: &WorkFacts) -> Value;          // truncates title 512, description 8192 at char boundaries
pub fn normalize_hint(raw: Option<&str>, options: &Options) -> String;            // a class string Settings offers, else ""
pub fn route_from_input(input: &Value, options: &Options, artifact_digest: &str, source_revision: u64, ended_attempts: u32)
    -> Result<aq_routing::Route, RouteRefusal>;
pub enum RouteRefusal { NotOffered(String), Invalid(String) }
```

- Facts sources:
  - `kind` is `aq:task_type`; `level_hint` is `aq:intelligence_hint` (`agent_access/human/enqueue.rs:29-30`).
  - `charged_failures` counts the task's earlier attempts that failed and were charged. Attempts released uncharged (`aq_machine_control::lost`, `charged = false`) and owner or service stops do not count.
  - `ended_attempts` is every ended attempt.
  - `route` comes from `CompactRoute::from_metadata`.
- `work.route`:
  - `level` and every listed harness and provider must be offered for that level; otherwise `refused not_offered`.
  - An owner route gives `refused owner_route`.
  - A revision mismatch or `Written::Stale` gives `stale`.
  - `policy_digest` is the active artifact digest of binding `aq.routing`; `hold: true` gives `provider_intent: pinned`.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test] async fn work_read_returns_the_facts_of_a_task_in_another_project_on_the_router() // R1
#[tokio::test] async fn work_read_refuses_a_task_whose_binding_is_pinned_to_a_profile()
#[tokio::test] async fn a_route_for_an_older_task_revision_is_stale_and_writes_nothing()       // Review Focus 1
#[tokio::test] async fn a_route_naming_an_unoffered_level_or_harness_is_refused()
#[tokio::test] async fn an_owner_route_is_never_replaced()
#[tokio::test] async fn the_route_records_the_artifact_digest_and_hold_pins_the_provider()
#[test] fn facts_never_include_priority()                                                      // read_output keys
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** the logic in `host9.rs`, with only the reads and the dispatch in `host_adapters.rs`.
- [ ] **Step 4: Run** the module, `-p aq-playbook-domain -p aq-routing`, and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(playbooks): task facts and route writes for playbooks (Host9)`

### Task B4: `judgment.assess`

After B3 (same dispatch file).

**Files:**
- Modify: `crates/aq-playbook-domain/src/host9.rs` (schema and answer conversion), `crates/aq-server/src/playbooks/internal_judgments.rs` (`assess`; purpose by source), `crates/aq-server/src/playbooks/host_adapters/requests.rs` (dispatch; completion through the existing judgment completion path, emitting `aq.judgment.assessed`)
- Test: unit tests in `host9.rs`; `tests/it_playbook/playbook_host9.rs`

**Interfaces:**
- Consumes: `aq_playbook_authoring::run_for_artifact` (C1).
- Produces:

```rust
pub fn assess_schema(input: &Value) -> Result<Value, String>;        // draft 2020-12; additionalProperties false; each field an enum; flags boolean; rationale maxLength
pub fn assess_output(answer: &Value, input: &Value) -> Result<Value, String>; // labels in field order; flags that are true, in request order
```

- Purpose: `routing_classification` when the run's artifact belongs to a source whose `judgment_purpose` says so, otherwise `playbook_call`.

- [ ] **Step 1: Write the failing tests**:
  - `an_assessment_answers_by_name_and_reaches_the_playbook_as_labels_and_flags`;
  - `an_answer_outside_the_choices_fails_as_invalid_output`;
  - `a_routing_playbook_judgment_runs_with_the_routing_classification_purpose`;
  - `a_request_over_sixteen_fields_is_refused`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the module and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(playbooks): structured judgments (judgment.assess)`

### Task B5: The server originates `aq.work.route_needed`

After B3.

**Files:**
- Modify:
  - `crates/aq-playbook-domain/src/host9.rs` (`route_needed_event`);
  - `crates/aq-server/src/playbooks/triggers.rs` (`queue_host_event`: the `queue_occurrence` body, for Host9);
  - `crates/aq-server/src/routing.rs` (`route_needed` called from `placements` for `Placement::Wait`, and for a router route with `a < ended_attempts`, which becomes `Placement::Reroute`);
  - `crates/aq-routing/src/lib.rs` (`Placement::of` takes `ended_attempts`);
  - `crates/aq-server/src/profiled_execution/producer/workflow.rs:512-540` (pass the counts).
- Test: `tests/it_playbook/playbook_host9.rs`

**Interfaces:**
- Produces:

```rust
pub fn route_needed_event(task: Uuid, version: u64, reason: &str, now_ms: i64, contract: &EventContract) -> EventEnvelope; // event_id = uuid5(task, version, now_ms / 120_000)
pub(crate) async fn queue_host_event(&self, event: &EventEnvelope) -> Result<()>;                                       // Host9 only
```

- Throttle: the `route_needed_emissions` row (0120) is upserted. A task is emitted again only after 120 000 ms, or at once when `task_version` changes.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test] async fn an_unrouted_ready_task_emits_one_event_and_not_again_within_two_minutes()
#[tokio::test] async fn a_new_task_version_emits_at_once()
#[tokio::test] async fn a_task_retried_after_routing_is_routed_again()               // ended_attempts grew -> reason "retried"
#[tokio::test] async fn nothing_is_emitted_when_routing_is_off_or_the_binding_is_pinned()
#[tokio::test] async fn the_event_starts_a_run_of_an_installed_artifact_that_triggers_on_it() // first trigger on a host profile
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the module, `-p aq-routing`, and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(routing): the server originates route_needed`

### Task C3: Authoring workspace, request and image

After B2, C1, C2.

**Files:**
- Create: `crates/aq-playbook-authoring/src/request.rs`
- Modify: `crates/aq-playbook-authoring/Cargo.toml` (vendored `aq-playbooks-compiler`, `aq-playbooks-model` `=0.2.0`), `crates/aq-playbook-authoring/src/runs.rs` (submit), `Dockerfile.supervisor-host` (build `aq-playbooks` with `--manifest-path vendor/aq-playbooks-<rev>/Cargo.toml -p aq-playbooks-cli`, under the same `touch` step; copy to `/usr/local/bin/aq-playbooks`)
- Test: unit tests in `request.rs`; `tests/it_playbook/playbook_authoring.rs`

**Interfaces:**
- Consumes: `docs::{GUIDE, LANGUAGE, FORMAT, example_files}`, `draft::unfill`, `CatalogProfile::Host9.registry()`, `Host::{markdown, facts}`.
- Produces:

```rust
pub fn workspace_files(record: &SourceRecord, markdown: &str, catalog: &Registry, facts: &BTreeMap<String, Value>,
    previous: Option<&PreviousPlaybook>) -> Result<BTreeMap<String, String>>;
pub fn request(record: &SourceRecord, files: BTreeMap<String, String>, revision: u64) -> aq_internal_work::Request;
pub const INSTRUCTIONS: &str;    // generic; never mentions routing
pub const OUTCOME_SCHEMA: &str;  // JSON Schema of out/outcome.json (kind saved|question|unable)
```

- Files: `source.md`; `record.json` (the A3 `SourceRecord` and `facts_version`); `catalog.json` (the Host9 registry kept to `commands` + `events`, via `retain_registry`); `facts.json`; `GUIDE.md`; `LANGUAGE.md`; `FORMAT.md`; `examples/…`; and, when present, `previous/source.md`, `previous.artifact.json`, `previous/draft.json`, `previous/cases/*.json`.
- Request: purpose `PlaybookCompilation`, `deadline_ms 1_800_000`, `max_output_bytes 1_048_576`, `escalate true`, `affected {kind: "playbook_source", id: source}`, `agent {max_turns: 200}`.

- [ ] **Step 1: Write the failing tests**:
  - `the_workspace_offers_only_the_commands_the_record_allows`;
  - `the_instructions_and_guide_never_mention_routing` (case-insensitive `rout` absent from `INSTRUCTIONS`; the guide check runs in C5);
  - `a_second_run_gets_the_previous_playbook_and_cases`;
  - `submitting_records_the_internal_task_on_the_run`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Check the Docker build with `docker build -f Dockerfile.supervisor-host --build-arg CARGO_PROFILE=ci-image .`; `docker run --rm <image> aq-playbooks --help` must list `check try compare save ask unable`.
- [ ] **Step 4: Run to verify they pass**, plus `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(authoring): the workspace an authoring agent is given`

### Task C4: Save-time checks, taking effect, grants

After C3.

**Files:**
- Create: `crates/aq-playbook-authoring/src/save.rs`, `crates/aq-playbook-authoring/src/deploy.rs` (the `Deployer` trait and grant renewal), `crates/aq-server/src/playbook_authoring.rs` (the `Deployer` and `Host` impls over `Driver`, `artifacts::{install, activate, rollback, validate}` and `Service::grant`)
- Modify: `crates/aq-playbook-authoring/src/runs.rs` (`completed`, `resume`, `serve`)
- Test: unit tests in `save.rs`; `tests/it_playbook/playbook_authoring.rs`

**Interfaces:**
- Produces:

```rust
pub enum Ended { Saved(Saved), Question { question: String, line: u32, sentence: String },
    Unable { reason: String, line: u32, sentence: String }, Refused(String) }
pub fn check_saved(outcome: &Value, ws: &Workspace, facts_now_digest: &str, grant_capabilities: &BTreeSet<String>)
    -> Result<Saved, String>;        // seal again; Artifact::verify; artifacts::validate-equivalent registry check; caps ⊆ grant; run_cases pass; facts digest equal
pub trait Deployer: Send + Sync {
    async fn install(&self, artifact: &Artifact) -> Result<()>;
    async fn activate(&self, source: &str, binding: &str, digest: &str) -> Result<()>;     // ensures the R2 grant
    async fn rollback(&self, source: &str, binding: &str) -> Result<String>;
    async fn renew(&self, source: &str, binding: &str) -> Result<()>;                       // every 12 h
}
pub async fn completed<H: Host, D: Deployer>(host: &H, deployer: &D, internal_task: Uuid, outcome: &aq_internal_work::Outcome) -> Result<()>;
pub async fn reinstall_active<D: Deployer>(pool: &PgPool, deployer: &D) -> Result<usize>;  // at start
```

- The `comparison` against the previous artifact and the cases table (from `run_cases` on the saved artifact) are stored on the run.
- A save from a run that is no longer the newest is stored `superseded` and never activated.

- [ ] **Step 1: Write the failing tests** (internal work answered by a fake agent native that writes a prepared outcome):

```rust
#[tokio::test] async fn a_saved_playbook_is_installed_and_activated_and_the_previous_kept_for_rollback()
#[tokio::test] async fn a_save_whose_case_does_not_run_as_stated_is_refused()              // acceptance 6
#[tokio::test] async fn a_save_needing_an_ability_the_grant_does_not_give_is_refused()
#[tokio::test] async fn a_save_from_a_superseded_run_is_recorded_and_never_activated()   // Review Focus 2
#[tokio::test] async fn a_save_whose_facts_changed_is_refused_and_authoring_starts_again() // Review Focus 3
#[tokio::test] async fn a_question_or_unable_changes_nothing_and_keeps_the_sentence()      // acceptance 5
#[tokio::test] async fn rollback_reactivates_the_previous_artifact()
#[tokio::test] async fn grant_renewal_reactivates_without_growing_the_rollback_stack()
#[tokio::test] async fn after_restart_the_active_authored_artifact_still_receives_its_trigger_event() // Review Focus 5
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the module and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(authoring): check at save, take effect, keep the previous playbook`

### Task C6: Authoring HTTP API

After C4.

**Files:**
- Create: `crates/aq-playbook-authoring/src/http.rs` (generic over `Host`, owner extractor and error, like `aq-internal-work/src/http.rs:19-40`)
- Modify: `crates/aq-server/src/lib.rs` (nest the routes; spawn `runs::serve` and the renewal loop), `crates/aq-server/src/playbook_authoring.rs`
- Test: `tests/it_playbook/playbook_authoring.rs`, `tests/it_dashboard/supervisor_turns/admin.rs` (a supervisor reaches them through the admin nest)

**Interfaces:**
- Produces (owner, or supervisor as its approving owner):
  - `GET /api/playbooks/sources` → `{items: [SourceView]}`.
  - `POST /api/playbooks/sources {id, binding, purpose, markdown, commands, events}` → registers the source and starts a run.
  - `GET /api/playbooks/sources/{id}` → `SourceView`.
  - `PUT /api/playbooks/sources/{id}/markdown {markdown, expected_revision}`; refused for `builtin`.
  - `POST /api/playbooks/sources/{id}/runs` → author again.
  - `POST /api/playbooks/sources/{id}/rollback`.
- `SourceView = {id, kind, binding, active: {artifact, since_ms, comparison: {change: "none"|"wording"|"behaviour"|"abilities"}, cases: [{name, situation, result, passed}]} | null, latest: {run, status, question, unable, refusal, internal_task_id, created_at_ms, finished_at_ms} | null}`.
  - `result` is the case's final mutation call input, rendered as `key: value` pairs.

- [ ] **Step 1: Write the failing tests**:
  - `a_registered_source_is_authored_and_its_view_shows_the_cases`;
  - `a_supervisor_can_register_and_rerun_but_not_as_a_worker`;
  - `put_markdown_on_a_builtin_source_is_refused`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the modules and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(authoring): sources API`

### Task D2: Routing on the authoring path

After B5 and C6.

**Files:**
- Modify:
  - `crates/aq-server/src/routing.rs` (the built-in `routing` record; facts provider `routing_options` → `{levels, offers, harnesses, providers}` from `options()` in rank order; `set_enabled(true)` only with an active `routing` run);
  - `crates/aq-server/src/routing/http.rs` (`GET/POST /api/settings/intelligence/routing` → `{enabled, source: SourceView}`);
  - `crates/aq-server/src/profile_catalog/settings.rs:60-70` (a structure save calls `aq_playbook_authoring::requested(host, "routing")`);
  - `crates/aq-routing/src/{lib.rs,classify.rs}` (`Route.classification: Option<serde_json::Value>`, at most 8 KiB; remove `Classification`, `Risk`, `Flag`, `schema`, `REASON_MAX_CHARS`).
- Create: `crates/aq-server/tests/it_playbook/fixtures/routing/` (the appendix as a complete workspace: draft and 12 cases, one per kind group, risk and hint path), `tests/it_playbook/playbook_routing.rs` (new module)

**Interfaces:**
- Consumes: B3–B5, C1–C6.
- Produces: `routing::ROUTING_SOURCE: &str = "routing"` and `routing::BINDING: &str = "aq.routing"`, plus the fixture, which D4 and F1 reuse.

- [ ] **Step 1: Write the failing tests** (the fixture is sealed with `draft::seal` against Host9 and saved through a fake agent):

```rust
#[tokio::test] async fn saving_the_routing_document_starts_one_authoring_run()
#[tokio::test] async fn a_settings_change_that_changes_the_facts_authors_again()
#[tokio::test] async fn with_the_fixture_active_each_kind_is_routed_as_the_document_says() // acceptance 2, fake model answers
#[tokio::test] async fn a_classifier_outage_still_routes_standard_high_on_claude_or_codex_with_risk_assumed()
#[tokio::test] async fn the_switch_turns_on_only_with_an_active_routing_playbook()
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `it_playbook`, `it_access` (`routing::`), `it_profiled` (`profile_catalog::`) and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(routing): the routing document is authored into the playbook that routes`

### Task D3: Routing failures follow the internal work failure path (0011 §4 W6)

After D2.

**Files:**
- Modify: `crates/aq-internal-work/src/store.rs` (`record_failure`, `resolve_failures`), `crates/aq-server/src/routing.rs` (the three conditions), `crates/aq-server/src/playbooks/host_adapters.rs` (`work.route` `not_offered` calls `record_failure`)
- Test: `tests/it_playbook/playbook_routing.rs`, `tests/it_dashboard/dashboard_aggregation.rs`

**Interfaces:**
- Produces: `pub async fn record_failure(pool: &PgPool, purpose: Purpose, affected: &Affected, reason: Reason, detail: &str, now_ms: i64) -> Result<Uuid>`. It inserts a `failed` internal task with `escalate = true` and no candidates, unless an unresolved one exists for the same affected item and detail.
- `pub async fn resolve_failures(pool: &PgPool, affected: &Affected, now_ms: i64) -> Result<u64>`, called when a route is written.
- Conditions, all with purpose `RoutingClassification` and `affected {kind: "task", id, project_id}`:
  - unrouted for 600 000 ms while routing is on: `DeadlineExceeded`, "unrouted for 10 minutes";
  - a routing run that ended `failed`: `ProviderError`, "the routing playbook failed for this task";
  - `not_offered`, or placement finds no candidate for the route: `NoCandidate`, "<reason>".
  - These are raised by the server, never by the playbook.

- [ ] **Step 1: Write the failing tests**:
  - `an_unservable_route_is_messaged_to_the_project_supervisor_once`;
  - `a_task_unrouted_for_ten_minutes_is_escalated_once`;
  - `a_later_route_resolves_it_and_removes_the_attention_item`;
  - `one_classifier_failure_raises_nothing`;
  - in `it_dashboard`: `owner_escalated_routing_failure_points_at_the_task`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the modules and `python3 scripts/build-budget.py`.
- [ ] **Step 5: Commit** `feat(routing): routing failures use the internal work failure path`

### Task D4: One identity filler; retire the server routing pilot

After B1 and D2.

**Files:**
- Modify: `crates/aq-server/src/playbooks/coding.rs:431-463` (`Builder::seal` derives its registry and capabilities with `aq_playbooks_compiler::draft::retain_registry`), `crates/aq-server/tests/it_playbook/{routing_scale.rs,real_adapter_pilots.rs}` (the routing cases and the scale run use the D2 fixture on Host9)
- Delete: `crates/aq-server/src/playbooks/coding/pilots.rs:52-206` (`routing()`) and its test uses
- Test: `src/playbooks/coding/tests.rs`, `it_playbook`

- [ ] **Step 1: Write the failing test**: `coding::tests::seal_keeps_every_pinned_digest` asserts the D11 digests (`docs/implementation/D11.md:196-203`: `sha256:d6f7502c…ec21`, `sha256:d77c03d8…7c47`, `sha256:6e9af4fc…a9c8`), so the refactor cannot change them. It passes before the refactor, as a guard. Add `routing_scale` compile coverage on the fixture.
- [ ] **Step 2: Refactor and delete.**
- [ ] **Step 3: Run** `timeout 900 cargo nextest run -p aq-server --test it_playbook` and the lib tests `coding::`. Expected: PASS with the same digests.
- [ ] **Step 4: Run** `python3 scripts/build-budget.py`. Expected: a lower count.
- [ ] **Step 5: Commit** `refactor(playbooks): one identity filler; the Host8 routing pilot is gone`

---

## Dashboard tasks (aq-dashboard)

### Task E1: The routing editor shows authoring, cases, diff and rollback

After C6 and D2 (it can start on mocks once C6's shapes are merged).

**Files:**
- Modify: `src/api/routing.ts` (adapter for `{enabled, source: SourceView}`, display limits kept), `src/features/settings/RoutingPolicyEditor.tsx` (replace `CompiledPolicy` :13-53), `src/mocks/intelligenceStructure.ts` (labelled fixtures for running, saved, question, unable and refused)
- Create: `src/api/playbookSources.ts` (the `SourceView` adapter and rollback), `src/features/settings/PlaybookSourceStatus.tsx` (+ `.module.css`, `.test.tsx`)
- Delete: the old `RoutingActive`/`RoutingLatest` types and the `routing-compile-failed` scenario

**Interfaces:**
- Produces: `<PlaybookSourceStatus source={SourceView} onRollback={…} />`, a generic component with no routing words.
  - Running: "Writing the playbook…", with `<InternalRun affectedKind="playbook_source" affectedId={id} />`.
  - Saved: "In effect since …", the change ("No change", "Wording only", "Behaviour changed", "Abilities changed"), and a table of situation → result with a pass mark.
  - Question: "The playbook needs an answer", with the question and the quoted sentence.
  - Unable: "This sentence cannot be carried out", with the sentence and the reason.
  - Refused: the reason.
  - "Roll back to the previous playbook" appears when a previous artifact exists.
  - The editor keeps the switch, enabled only with `source.active`.

- [ ] **Step 1: Write the failing tests**:
  - `shows each state in plain words`;
  - `the cases table renders situation and result`;
  - `rollback calls the API and refreshes`;
  - `never shows a step id or a sha256 to the owner` (no `/sha256:/` or `/steps\./` in rendered text);
  - `the switch is disabled without an active playbook`.
- [ ] **Step 2: Run** `pnpm test -- PlaybookSourceStatus RoutingPolicyEditor`. Expected: FAIL.
- [ ] **Step 3: Implement**, following `docs/design/manifest.json` for the Intelligence settings page.
- [ ] **Step 4: Run** `pnpm typecheck && pnpm lint && pnpm test`. Review desktop and phone screenshots of Settings > Intelligence (`docs/design/visual-acceptance.md`).
- [ ] **Step 5: Commit** `feat(settings): the routing playbook's authoring status, cases, diff and rollback`

### Task E2: The internal run view shows what the agent did

After C2.

**Files:**
- Modify: `src/features/internal/InternalRun.tsx` (+ test), `src/api/internalWork.ts` (transcript adapter, bounded), `src/mocks/internalWork.ts`

- [ ] **Step 1: Write the failing tests**: `shows the agent's steps for an agent run`; `clips long tool output and shows the outcome`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement**: a list of steps (tool name, what it was asked, result clipped), then the outcome.
- [ ] **Step 4: Run** `pnpm typecheck && pnpm lint && pnpm test`.
- [ ] **Step 5: Commit** `feat(internal): show an authoring agent's steps`

---

## Acceptance

### Task F1: Automated acceptance and the inventory check

Last. Server repo, reading both repos.

**Files:**
- Create: `scripts/check-authoring-inventory.sh`, `docs/implementation/0013-playbook-authoring.md` (evidence for acceptance 2, 5, 6 and 7; the inventory result)
- Modify: `tests/it_playbook/playbook_routing.rs` (acceptance 2 across every kind named in the fixture), `tests/it_playbook/routing_scale.rs` (acceptance 7 on the authored fixture), `build-budget.toml` (`python3 scripts/build-budget.py --update`)

**Interfaces:**
- `check-authoring-inventory.sh ENGINE_DIR` exits 1 on any finding.
- It fails if any of these exist:
  - `aq_routing::(policy|plan|compile|fixtures)`, `COMPILER_INSTRUCTIONS`, or `routing_policies` outside `migrations/012[01]*`;
  - `unsupported_features` or `Severity::Question`;
  - `Command::(Surface|Interfaces|Simulate)`, or `pub mod (fake_host|pilots)` under `crates/aq-playbooks-cli/src`;
  - `pub fn routing()` in either `pilots.rs`;
  - `struct Classification` in `aq-routing`.
- It also fails unless each REPURPOSE symbol has a caller on a tool or save path:
  - `draft::seal` in `crates/aq-playbooks-cli/src/authoring.rs` and `crates/aq-playbook-authoring/src/save.rs`;
  - `run_cases` in the same two files;
  - `semantic_diff` in `draft.rs::compare`;
  - `parse_source_header` reached from `validate`;
  - `artifacts::validate` (or its equivalent call) in `playbook_authoring.rs`;
  - `retain_registry` in `coding.rs` and `request.rs`;
  - `route_needed_emissions` in `routing.rs`.

- [ ] **Step 1: Write the script** with the checks above, and run it against both repos. Expected: no findings. Fix any finding in the task that owns it.
- [ ] **Step 2: Run** `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^playbook_routing::|^playbook_authoring::/)'`. Expected: PASS (acceptance 2, 5, 6).
- [ ] **Step 3: Run acceptance 7 by hand**: `eval "$(scripts/test-db)"; timeout 3600 cargo test --release -p aq-server --test it_playbook -- --ignored routing_scale --nocapture`. Expected: the slowest tick at run 500 is within 1.2× of run 10. Record the numbers and the machine in the note.
- [ ] **Step 4: Run** `python3 scripts/build-budget.py --update` (it only lowers), then the full suite `timeout 900 cargo nextest run --workspace --locked --no-fail-fast`, and the engine's `cargo test --locked --workspace`.
- [ ] **Step 5: Commit** `test(authoring): acceptance evidence and the inventory check`

### Task F2: Live acceptance runbook (written, not executed)

Last. Server repo, documentation only.

**Files:**
- Create: `docs/operations/0013-live-authoring-runbook.md`
- Modify: `scripts/dev-stack.py:182-268` (profile `host_v9`, a fresh namespace generation, `installation_routing: true`, internal-work agent mode with the image's `aq-playbooks`), `docs/status.md` (what is built, and that the live check is pending)

- [ ] **Step 1: Write the runbook** for the operator. For each step it gives the command or dashboard action, the expected observation, and the rollback. It never touches `aq-ms1-*` containers outside the steps the owner approves.
  1. Owner approval: R1 and R2, the migrations 0121–0123 (forward-only), and the namespace generation change.
  2. Deploy, then check `aq-playbooks --help` in the internal-work container.
  3. Settings: set a Claude-first profile override for `playbook_compilation`.
  4. Save the routing document unchanged; within 30 min the view shows "In effect" and the cases table (acceptance 1). Record the engine verifier's acceptance with `aq-playbooks check` on the stored draft.
  5. Turn routing on. Create tasks of each kind the document names, and record each task's route (acceptance 2).
  6. Add "Ask for a second opinion on high-risk work." to the document and save. The diff says behaviour changed and the cases table gains rows (acceptance 3).
  7. Register the review-reminder source through `POST /api/playbooks/sources`. Start it with `POST /api/playbooks/admissions` on a review task, and see the message (acceptance 4).
  8. Add "Post the route to the team chat." and save. The view shows that sentence as unable, and the earlier playbook still routes (acceptance 5).
  9. Record tick time before and after.
- [ ] **Step 2: Check the dev stack**: `python3 scripts/dev-stack.py --help` and `scripts/product-smoke.py` in fixture mode.
- [ ] **Step 3: Commit** `docs(operations): live acceptance runbook for playbook authoring`

---

## Self-review

- **Spec coverage.**

  | Spec | Tasks |
  | --- | --- |
  | §4.1 | C1, C6, D2 |
  | §4.2 | C2, C3, C5 |
  | §4.3 | A5 `save`, C4 |
  | §4.4 | A5, C4, E1 |
  | §4.5 | C4, E1 |
  | §5.1 | A1 |
  | §5.2 | A2, A5 |
  | §5.3 | A4, B4 |
  | §5.4 | A3, A5 |
  | §5.5 | C5 |
  | §6 | A4, B2–B5 (§6.2 deliberately not built: Spec check 7) |
  | §7 | D2, D3 |
  | §8 | D1, D2, D4 |
  | §9.1–9.7 | F2 (1, 3, 4, live 2 and 5), D2 and F1 (2), C4 (5, 6), F1 (7) |
  | Owner's inventory rule | Spec check 11 + F1 |

- **Type consistency.**
  - `SourceRecord` exists twice on purpose: the engine's (A3) is the workspace file; the server's (C1) is the stored record. C3 converts the server's into the engine's.
  - `Case` and `run_cases` come from A5 and are used in C4 and F1. `retain_registry` comes from A3 and is used in C3 and D4. `CompactRoute.a` comes from B3 and is used in B5.
- **Ratchet.** D1 frees about 430 lines. Glue in B2–B5, C4, C6, D2 and D3 is estimated at about 350 lines; D4 frees about 150.
