# 0004 — Playbook authoring: implementation notes and engine 0.2.0

Engine 0.2.0 joins the authoring line (`feat/playbook-authoring`) with the
storage line's release (`5de3049`: spec 0003, runner and postgres 0.2.0).
Every workspace crate is `0.2.0` and the root pins are `=0.2.0`:
`aq-playbooks-model`, `-compiler`, `-runner`, `-postgres`, `-simulation`,
`-cli`. The authoring line has no spec file of its own; this note, the guide
and `LANGUAGE.md` are its record. Storage is described in
`docs/specs/0003-live-state-only.md` and
`docs/implementation/0003-live-state-only.md`.

## What the authoring line added

- **Draft language** (`crates/aq-playbooks-model/src/draft.rs`,
  `crates/aq-playbooks-compiler/src/draft.rs`). Expressions `lookup` and
  `let` (`Expr`, `LookupCase`, `LetBinding`, `MAX_LOOKUP_CASES`,
  `MAX_LOOKUP_KEY_BYTES`, `MAX_LET_BINDINGS`), and reads of a named join's
  answers. Reference: `crates/aq-playbooks-compiler/LANGUAGE.md`.
- **Draft workspace** (`draft::Workspace`): `source.md`, `record.json`,
  `catalog.json`, `facts.json`, `draft.json`. A draft names contracts and
  dependencies; `draft::fill` puts the identities in (id, scope,
  capabilities, `{name, fingerprint}`), `draft::check` validates,
  `draft::seal` produces the artifact, `draft::unfill` and `draft::compare`
  go back to the owner's terms.
- **Host catalog 2.2.0** (`contracts/aq-host-2.2.0/`): commands `work.read`,
  `work.route`, `work.unroutable`, `judgment.assess`; events
  `aq.judgment.assessed`, `aq.work.route_needed`. `notes.json` holds what the
  host says about every input field, outcome and event.
- **Simulation crate** (`crates/aq-playbooks-simulation`): the scripted
  simulator that was `aq_playbooks_cli::simulation`, plus authoring cases
  (`cases.rs`: `Case`, `run_case`, `run_cases`, `Outcome`, `quote`,
  `facts_digest`). A host can call it to re-run an author's cases.
- **Authoring CLI** (`crates/aq-playbooks-cli/src/authoring.rs`, run in a
  workspace directory, JSON on stdout): `check`, `seal [--output]`,
  `try <case>`, `compare`, `facts`, `save`, `ask`, `unable`. `save`, `ask`
  and `unable` end the run by writing `out/outcome.json`.
- **Documents shipped to the agent** (`aq_playbooks_compiler::docs`): the
  guide `skills/aq-playbook-author/SKILL.md` (`docs::GUIDE`),
  `crates/aq-playbooks-compiler/LANGUAGE.md` (`docs::LANGUAGE`), and
  `CATALOG.md`, rendered by `docs::catalog` from the 2.2.0 registry and
  `contracts/aq-host-2.2.0/notes.json`.
- **Worked examples**
  (`crates/aq-playbooks-compiler/authoring-examples/escalate-on-judgment`,
  `.../notify-when-ready`): complete workspaces with cases and a `stuck/`
  ending each. `docs::example_files()` embeds them; `aq-playbooks examples
  --output DIR` writes them.

## API differences for the server

From `git diff e7df379 HEAD` (e7df379 is what aq-server vendors today).
"Breaks" means code written against e7df379 stops compiling or behaves
differently; "new" is additive.

### Workspace and versions

1. Breaks. Every crate is `0.2.0` (runner, postgres, cli from `0.1.3`; model,
   compiler from `0.1.0`), with exact pins `=0.2.0` in the root
   `[workspace.dependencies]`. The runner now pins the model
   (`aq-playbooks-model = { path = "../aq-playbooks-model", version = "=0.2.0" }`).
   Model and compiler state `version = "0.2.0"` themselves (no longer
   `version.workspace = true`).
2. Breaks. New workspace member `crates/aq-playbooks-simulation`; the CLI
   depends on it (`aq-playbooks-simulation.workspace = true`).
3. Breaks. `COMPILER_VERSION` is now `"aq-playbooks-compiler/0.2.0"`. It is
   part of the artifact content, so the same draft seals to a different
   digest than under 0.1.0. The executable fingerprint of an artifact
   without dependencies is unchanged; one with dependencies names them by
   digest, so its fingerprint changes too.
   Artifacts already stored keep their stamp and digest and still load. A
   host that re-seals to compare with a recorded digest must use the same
   engine version as the author.

### Runner (`aq-playbooks-runner`)

4. Breaks. `RetentionPolicy` has seven new fields: `drain_cohorts_per_commit`
   (50), `drain_entries_per_commit` (500), `window_drops_per_commit` (64),
   `replay_evictions_per_commit` (64), `history_trim_per_commit` (64),
   `gc_page_limit` (1000), `settle_cohorts_per_commit` (50). A struct literal
   without `..Default::default()` no longer compiles. Stored policies and
   JSON without them decode to the defaults.
5. Breaks. `Engine::set_retention_policy` refuses a policy with any of these
   at 0: `Error::Refused("drain bounds must be positive")` or
   `Error::Refused("per-commit retention bounds must be positive")`.
6. Breaks. `Engine::collect_garbage(now_ms, limit)`: `limit` must be
   `1..=policy.gc_page_limit` (was the constant 1000; the refusal text is
   now `GC page limit must be 1..{gc_page_limit}`). It is one ordinary commit
   plus a page of artifact disposal; nothing requires calling it.
7. Breaks. `GcReport` gains `admissions` and `evicted` (both
   `#[serde(default)]`); `runs`, `events`, `receipts` now report what the
   commit moved to history or forgot, `preserved` counts artifacts only, and
   `more` also covers the settle, drain, window and eviction work.
8. Breaks. `history::trim_budget(count, written, policy: &RetentionPolicy)`
   (third argument was `keep: u64`).
9. Breaks. `retention::verify_removals(previous, next)` returns
   `Result<(), StorageError>` (was `Result<Removals, _>`);
   `retention::Removals` is removed.
10. Breaks. `IndexQuery::Receipts` items: `IndexItem::key` is the receipt's
    sequence as 20 zero-padded digits (was its position in the record). A
    stored cursor from the old engine does not continue.
11. Breaks (behaviour). `RuntimeIndexes::root_last_activity` is no longer
    written (deprecated, read by nothing).
12. Breaks (behaviour). An uncertain asynchronous operation holds the events
    of its completion contract only until the retention clock passes its
    command deadline plus `max(policy.event_replay_ms, contract.retention_ms)`.
13. Breaks (behaviour). `Engine::new` on a State written before the
    `contract_events` index no longer fills it in one pass: the drain fills
    it, a bounded number of events per commit. A rebuild of the indexes
    resets the drain.
14. New. `RetentionState::drain: DrainState` and
    `RetentionState::rejudge: BTreeMap<String, u64>` (both defaulted on
    decode); `retention::DrainState { done, root_cursor, roots_done, phase,
    cursor, legacy_sequence }`, `DrainState::event_pending`, the phase
    constants `DRAIN_TOMBSTONES` .. `DRAIN_DONE`, `retention::GC_PAGE_LIMIT`.
    A State written by 0.2.0 carries these fields; an e7df379 engine must not
    be pointed at it afterwards.
15. Unchanged, stated because it was asked: `Error` and `StorageError` have
    no new variant since e7df379 (`Error::Stale` was already there).

### Postgres (`aq-playbooks-postgres`)

16. Breaks (behaviour). `PostgresStorage::connect` runs `ANALYZE` on each
    engine table that has no planner statistics, outside any commit. A role
    that may not analyze a table gets a line on stderr and the table is
    skipped. No new migration (still 0001..0007).
17. Breaks (behaviour). No commit writes a `disposal_tombstones` row for a
    run, operation or event. Rows an older engine wrote are deleted by the
    drain, at most `policy.drain_entries_per_commit` per commit; only
    artifact rows remain in use. The artifact catalog delete of a GC commit
    is bounded by `policy.gc_page_limit`.
18. Breaks (behaviour). Receipt rows of the runtime index (`receipt`,
    `all_receipt`) are keyed by sequence (20 digits), matching item 10. Rows
    keyed by position leave when their receipt leaves.
19. Breaks (behaviour). History trim per commit follows
    `policy.history_trim_per_commit`, not the constant 64.

### Model (`aq-playbooks-model`)

20. Breaks (text only). The `binding` validation message names both steps
    (``step `a` binds `x`, which step `b` already binds``); the `draft_limit`
    message for shape work is reworded. No public item changed.

### Compiler (`aq-playbooks-compiler`)

21. New. Module `docs`: `GUIDE`, `LANGUAGE`, `CATALOG_REGISTRY`,
    `CATALOG_NOTES`, `catalog_registry()`, `notes(kind, name)`,
    `catalog(&commands, &events) -> Result<String, String>`,
    `short_type(&Schema)`, `example_files()`.
22. Breaks for a server that took an authoring commit between e7df379 and
    now: `docs::FORMAT` is gone (it did not exist at e7df379 either). The
    catalog document is rendered per source by
    `docs::catalog(&commands, &events)`; `FORMAT.md` is for engine developers
    and is not shipped to an agent.
23. Breaks. The compiler embeds files outside its crate at build time:
    `../../../skills/aq-playbook-author/SKILL.md` and
    `../../../contracts/aq-host-2.2.0/{registry,notes}.json`. The vendored
    tree must keep `crates/`, `contracts/` and `skills/` as siblings.
24. Breaks (behaviour). `draft::seal` validates before it compiles and
    returns the placed faults.
25. Breaks (behaviour). New check code `deadline` from `draft::check` and
    `draft::seal`: a way from an entry whose `wait` and `join` deadlines add
    up to `limits.max_duration_ms` or more is refused (location
    `playbook.limits`). The code already existed for single steps.
26. Breaks (behaviour). New check code `catalog` from `draft::fill` (so from
    `check`, at stage `filling`): `record.json` allows a command or event
    that `catalog.json` has no contract for. No draft can fix it; the agent
    is told to end with `unable --file catalog.json`.
27. Breaks (text only). The `unknown_event` message says "an event".

### Simulation (`aq-playbooks-simulation`, new crate)

28. Breaks. `Start`, `Action`, `Scenario`, `ScriptedHost`,
    `SimulationReport`, `simulate`, `inspect` moved here unchanged from
    `aq_playbooks_cli::simulation`. The CLI re-exports the crate under the
    old path (`pub use aq_playbooks_simulation as simulation`).
29. New. `cases`: `Case`, `CaseStart`, `CaseReturn`, `Expectation`,
    `ExpectedCall`, `Call`, `CaseReport`, `run_case(&artifact, &case)`,
    `run_cases(&artifact, &cases, total_transitions)` (refuses fewer than
    `MIN_CASES` 5 or more than `MAX_CASES` 40), `facts_digest`, `Sentence`,
    `quote(markdown, line, end_line)`, and the limits `MAX_CASE_NAME_BYTES`,
    `MAX_SITUATION_BYTES`, `MAX_RETURNS_PER_COMMAND`, `MAX_CASE_TRANSITIONS`,
    `CASES_TRANSITION_BUDGET`, `MAX_OUTCOME_BYTES`, `MAX_QUESTION_BYTES`,
    `MAX_REASON_BYTES`, `MAX_SENTENCES`, `MAX_SENTENCE_BYTES`,
    `MAX_SENTENCE_LINES`.
30. New. `Outcome` (`out/outcome.json`, `OUTCOME_FILE`; tagged by `kind`,
    unknown fields refused) with `OutcomeKind`, `kind()`, `check()`,
    `to_bytes()`, `from_bytes()`:
    - `Saved { draft, cases, artifact, facts_digest }`
    - `Question { question, sentences: Vec<Sentence> }`
    - `Unable { reason, line?, end_line?, sentence?, file? }`: either the
      sentence (`line`, optional `end_line`, `sentence`) or, for a fault in a
      host file, `file` (one of `HOST_FILES`: `record.json`, `catalog.json`,
      `facts.json`) and no sentence.
    Every field is the agent's claim: the host re-seals, re-runs the cases,
    recomputes `facts_digest` and re-quotes each sentence from its own copy.

### CLI (`aq-playbooks-cli`, binary `aq-playbooks`)

31. Breaks. Commands removed: `compile`, `validate`, `diff`, `interfaces`,
    `surface`, `simulate`.
32. New. Commands `check`, `seal`, `try`, `compare`, `facts`, `save`, `ask`,
    `unable` (run in the workspace directory; JSON on stdout; exit 1 on a
    refusal).
33. Breaks. `save`, `ask` and `unable` refuse when `out/outcome.json`
    already exists ("a run ends once"). A host that reuses a workspace must
    remove the file first. `seal --output` refuses to write over a host file
    or the outcome.
34. Breaks. `examples --output DIR` writes the worked authoring workspaces
    and prints `{"directory", "examples"}` (was the pilot artifacts and
    `{"directory", "pilots"}`).
35. Breaks. An argument error is `{"error": ...}` on stdout with exit 2
    (was clap's text on stderr). A refusal of `commands` or `events` is JSON
    on stdout too.
36. Breaks (behaviour). `commands describe` and `events describe` add a
    `"notes"` field when the contract is the pinned 2.2.0 one. With no
    `--registry`, `commands` and `events` read `catalog.json` of the
    workspace they run in.
37. Breaks. Library: `aq_playbooks_cli::fake_host` and
    `aq_playbooks_cli::pilots` are removed (test support now, under
    `crates/aq-playbooks-cli/tests/support/`). From
    `aq_playbooks_cli::catalog`: `strings`, `object`, `enumeration`, `text`,
    `revision`, `subject`, `result`, `work_result`, `list`, `CONTEXT_VIEWS`,
    `WORK_KINDS`, `CHANGE_VARIANTS`, `describe_surface` are removed. The
    versioned registries and manifests (`catalog*()`, `REGISTRY_*_JSON`,
    `MANIFEST_*_JSON`, `VERSION*`) stay.
38. Changed text only: `gc` and `retire-producer` help; `gc` still works.

## What a vendoring script must copy

Keep the layout: several crates read files by relative path at build time.

- `Cargo.toml` and `Cargo.lock` of the workspace (pins, `edition`, shared
  dependency versions; the crates inherit `edition` and dependencies from the
  workspace).
- `crates/aq-playbooks-model/`, `crates/aq-playbooks-compiler/` (with
  `LANGUAGE.md` and `authoring-examples/`), `crates/aq-playbooks-runner/`,
  `crates/aq-playbooks-postgres/` (with `migrations/`, `scripts/`,
  `examples/`), `crates/aq-playbooks-simulation/`,
  `crates/aq-playbooks-cli/` (with `tests/support/`).
- `contracts/` whole: `aq-host/`, `aq-host-2.0.0/`, `aq-host-2.1.0/`,
  `aq-host-2.2.0/` (`registry.json`, `manifest.json`, `fixtures.json`,
  `notes.json`, `README.md`). The CLI embeds every registry and manifest;
  the compiler and the simulation tests embed 2.2.0.
- `skills/aq-playbook-author/SKILL.md` (embedded by the compiler).
- `docs/specs/0003-live-state-only.md`,
  `docs/implementation/0003-live-state-only.md`,
  `docs/implementation/0004-playbook-authoring.md` (this note),
  `docs/aq-host-integration.md`, and `crates/aq-playbooks-model/FORMAT.md`
  (inside the model crate).

Tests read across crates (`crates/aq-playbooks-model/tests/fixtures/`,
`crates/aq-playbooks-model/FORMAT.md` and `src/`,
`crates/aq-playbooks-cli/src/authoring.rs`, `contracts/`), so a copy that
runs the engine's tests needs every crate whole, `tests/` included.

## Verification

`cargo fmt --check`, `cargo clippy --locked --workspace --all-targets -- -D
warnings`, `cargo test --locked --workspace`, and
`crates/aq-playbooks-postgres/scripts/test-postgres.sh` (a throwaway
PostgreSQL 17 container). The pinned pilot bytes
(`crates/aq-playbooks-model/tests/fixtures/*.artifact.json`) keep their
0.1.0 stamp: `the_coding_pilot_keeps_its_pinned_bytes` seals the pilots
under that stamp before it compares.
