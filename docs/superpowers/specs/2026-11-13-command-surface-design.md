# Command surface: summaries, read parameters, CLI stubs

**Status:** scope approved by the owner on 2026-11-13. Design calls made
without the owner are marked **(call)**.
**Repository:** aq-server (`crates/aq-command-registry`, `crates/aqv2`,
`protocol/commands/v1`).

## Why

An agent or a person discovering AQ commands gets too little from the
registry today (comparison of the v1 and v2 command surfaces, 2026-11-13):

- 32 of 43 mutations carry the summary "Dashboard action
  (protocol/dashboard-actions/v1)".
- 65 of 136 principal routes publish a schema that names only the path
  params and accepts any other scalar as a query value. A misspelled filter
  is silently ignored, and nothing lists the filters that exist.
- `aq tasks cancel`, `aq tasks retry` and `aq workers revoke` answer
  "unsupported" although `task.cancel`, `task.retry` and `worker.revoke` are
  registered commands.

The playbook authoring agent (spec 0013) depends on command discovery, so
this is done first and separately from that work.

## Goals

1. Every registered command has a one-line summary that says what it does.
2. Every read published with a generated schema lists its query params with
   types, and a param the schema does not list is refused before the request
   is forwarded.
3. The three stubbed owner commands run their registered commands.

Dropped during the build: hiding the plural groups (`tasks`, `messages`,
`operations`) from help. They are the owner-mode groups, not duplicates of
the singular channel-mode ones, `tasks list` and `messages inbox` have no
singular form, and an existing test requires them in help.

## Not goals

- Generated CLI flags or help from schemas, result schemas, `--brief`,
  pagination metadata, new command registrations. Each is a follow-up.
- Changing any REST route, handler or the dashboard.
- `workers enroll` and `projects onboard`, which have no registered command.

## Anti-goals

- Growing the aq-server library (line ratchet, AGENTS.md).
- Touching `ActionKind` implementations, which the in-flight playbook branch
  (`feat/internal-work`) is editing.

## Approaches

**Summaries.** (a) A `SUMMARY` constant on the `ActionKind` trait: the
summary sits next to the action, but it edits every action kind in the
aq-server library, against both anti-goals. (b) A table of kind and summary
in the registry crate, with a test that every registered action kind has an
entry. Chosen: (b). An action kind added without a summary still starts the
server (it keeps the placeholder) and fails the test. **(call)** The
playbook branch adds `task.delete`, `task.route.set` and `task.route.clear`;
whichever branch merges second adds their three lines.

**Read parameters.** (a) Derive each schema from the handler's query type
with `schemars`: exact, but it needs the registry crate to see aq-server's
handler types, or a derive on about 45 structs in the server library. (b)
Declare each route's query params in the catalog next to the route. Chosen:
(b). Its weakness is drift between the declaration and the handler; see
Review focus in the plan.

**Stubbed commands.** The actions take a subject with a revision. (a)
Require `--revision`. (b) Read the current revision when it is not given.
Chosen: both, `--revision` optional. **(call)** Without it the CLI reads the
subject first and sends the revision it saw, so the command still fails
cleanly if the subject changes between the two requests.

## Design

- `summaries.rs` in the registry crate: `action_summary(kind) ->
  Option<&'static str>`. `Registry::new` uses it for an action kind that has
  no catalog entry.
- `queries.rs` in the registry crate declares each GET route's query params,
  keyed by route path, and `Route::query()` reads it. **(call)** A table by
  path, not a field on `Route`, leaves `catalog.rs` untouched and gives a
  supervisor read (`/v1/supervisor/read/...`) the owner route's params by
  construction. A `QueryParam` is a name, a JSON Schema fragment and whether
  it is required. A param the handler accepts but ignores is not declared. The generated schema for
  a `Params::Path` route becomes path params plus declared query params with
  `additionalProperties: false`.
- `forward` refuses a `Body::Query` request that carries a param outside
  the path segments and the declared list. The owner approved refusing
  unknown params.
- `aqv2`: `tasks cancel|retry ID --project P [--revision R]` and `workers
  revoke MACHINE` send the registered action kinds through the action union
  (`POST /api/v1/actions`), as `tasks create` does. **(call)** `worker.revoke`
  takes no revision.
- `commands.json` is regenerated. Its digests change for every route whose
  schema changes.

## Left to the builder

- The exact wording of the summaries.
- The param list of each read, taken from its handler.
