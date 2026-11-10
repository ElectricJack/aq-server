# Intelligence Settings

The owner requested restoration of the v1 intelligence profile editor on 2026-11-09. Settings now has an Intelligence tab at `/settings?tab=intelligence` (the old `/settings/intelligence` route redirects there). A provider dropdown sits above a spreadsheet of profile names, descriptions, models and reasoning effort. Owners can edit rows and add new ones. Switching providers preserves the other mappings. Provisioning new harness/model configurations remains separate.

The owner API is GET/POST `/api/settings/intelligence`, with GET `/api/settings/intelligence/operations/{id}` for save recovery. The existing owner and CSRF guards apply. Reads include referencing projects and qualified model choices, with no credential material. Catalog revisions are decimal strings in the new API.

Saving publishes new immutable intelligence and referring agent versions. Version allocation includes previously known identities, so rollback cannot cause a version number to be reused with changed bytes. One transaction stores the catalog, preserves exact execution-template pins, updates all project bindings referencing the old active catalog, records binding history, advances activation, and stores the operation receipt. Project enabled/disabled flags and permissions are preserved; running attempts keep their original artifacts. A failure rolls everything back. Concurrent stale editors receive a conflict, while exact operation replay returns the original receipt. A no-op save records a receipt without republishing or repinning.

Profile display metadata is optional and omitted from old definitions, preserving their exact artifact identities. There is no new migration or separate editable database catalog. Each edited row captures its profile version, preserves a conflicting draft and requires operation inspection or an exact retry after an unconfirmed response. Intelligence drafts survive Settings tab changes. The six Settings tabs are Identity, Display, Intelligence, Connection, Projects and Notifications.

A save with profile version zero creates a new identity at version one. Empty candidate arrays produce the existing deliberately unconfigured placeholder convention, permitting named rows before provider setup without introducing a runnable choice. New profiles are not assigned to agents or project allowlists. Existing projects are repinned to the resulting catalog digest atomically so catalog consistency is maintained.

## Validation

- Existing `it_profiled` binary, profile catalog and inspection modules: 16 tests passed. Coverage includes actual owner routes and PostgreSQL, atomic publication/repinning, forced storage-failure rollback, concurrency, stale revisions, exact replay, unsupported choices, no-op saves, spreadsheet row creation, preserving existing choices, and the edited model/reasoning reaching future composition.
- `aq-profiles --tests`: 39 tests passed, including frozen schema/task compatibility, old golden artifact identities, compiler, registry and CLI tests.
- Frontend changed-section component checks: 54 tests passed, including provider preservation, adding rows, tab drafts, conflict preservation and interrupted-save reconciliation/retry. Four browser tests cover tabs, keyboard navigation, responsive composition and notifications.
- Production dashboard build, mock exclusion, typecheck and server format/Clippy checks passed. Integration binaries remain at 81; the server library ratchet falls from 109103 to 109100 lines. The main editor/storage logic and extracted inspection/read helpers live in `aq-profile-settings`.
- The initial labelled fixture forms were captured at 1440, 390 and 320 pixels with no horizontal overflow or accessibility violations. Shared Settings tokens/panels govern this page; Settings has no separate approved PNG. Desktop and phone captures were inspected. The revised spreadsheet and Settings tabs are qualified with fresh live desktop/phone captures. This is the delivered Settings subset, not full-dashboard acceptance or independent code review.

Live evidence and rollback details are stored locally under `/home/jkern/dev/aq-platform/qa/2026-11-09/intelligence-settings/`. Live qualification uses an unchanged save; changed profile publication and future composition are qualified on the disposable test database without model calls.

## Provider catalog and v1 defaults (owner followup)

The owner requested all active v1 defaults and a complete model/effort picker across their providers, preferably discovered through supported interfaces. Eleven active vault profiles are migrated through the owner save API, preserving names, descriptions, reasoning levels, Gemini budgets and service tiers. Retired/backup files are excluded. V1 `codex` slices normalize to OpenAI with the Codex harness; three equivalent OpenAI API aliases collapse into their identical Codex mappings. The original export remains in private evidence. Existing v2 profiles and agent assignments remain intact.

Settings preferences are separate from qualification: `intelligence_preferences_file` points at a bounded JSON metadata cache containing discovered choices and imported defaults. Runtime credential and qualified-harness configuration are unchanged. Each choice exposes its source, observation time, original provider options and `preference_only` flag. All saved choices still undergo independent selection/qualification before any launch. A failed or removed discovery entry does not erase saved profiles.

`scripts/refresh-model-catalog.py` refreshes the cache atomically, retaining the last successful snapshot per failed source. It submits no prompts and uses no API keys. Codex `model/list` supplies models and supported efforts; the official Claude Agent SDK `supportedModels()` supplies resolved models and supported effort levels; Gemini ACP supplies models when its existing login permits it; Ollama `/api/tags` and `/api/show` supply installed completion models and capabilities. OpenCode's published public catalog fills missing model/effort metadata, clearly marked as access-unverified. Metadata without reported levels offers provider default; token budgets use reported bounds and retain imported exact values. No guessing effort levels from model names.

References: [Codex App Server](https://developers.openai.com/codex/app-server/), [Claude SDK ModelInfo](https://code.claude.com/docs/en/agent-sdk/typescript#modelinfo), [Gemini ACP](https://geminicli.com/docs/cli/acp-mode/), [OpenCode models](https://opencode.ai/docs/models/), [Models.dev API](https://models.opencode.ai/providers/opencode/). Refresh is an explicit host metadata operation, not a model generation or Settings save; the dashboard reads the latest cache without restarting the daemon.

Example refresh (install the official Claude SDK locally and pass its sdk.mjs path):

```sh
python3 scripts/refresh-model-catalog.py --output /path/to/model-choices.json --claude-sdk /path/to/claude-agent-sdk/sdk.mjs
```

Validation adds a real PostgreSQL test for preference import with no runtime registration, cache changes without daemon restart, and preservation of native service options; five Python checks cover absent effort metadata, partial budget bounds, non-completion model exclusion and atomic replacement; eight Intelligence component tests pass. Build budgets remain 81 integration binaries and aq-server at 109094 lines.

## Row deletion and automatic routing (owner requirement)

Saved rows can be deleted even when agent definitions or projects use them. Deletion removes the explicit assignment and returns future work to AQ's normal model-selection router. It never asks the owner to manually detach references. The mutation publishes new versions of affected agent definitions, updates project agent/intelligence references atomically, and preserves execution profiles, permissions, enabled flags and running-session artifacts.

The router uses qualified configured model choices and the existing AQ selection engine. Its internal versioned definition carries `automatic_routing: true` and stays out of the editable profile spreadsheet; it is routing infrastructure rather than an extra placeholder row. Existing router inventory is retained when no fresh qualified choices are available. Ordinary runtime credential/worker observations still govern selection. This does not migrate v1's separate task-classification playbook.

Receipts report automatic routing and affected agent identities. Optional metadata and delete flags remain omitted from older definitions/requests, preserving old artifact identities and operation replays. Dependency updates use distinct agent and intelligence namespaces. Historical artifacts and exact environment pins remain available.

Validation: 20 catalog/inspection tests cover agent/project fallback, last-visible-profile removal, qualified model choices, same-name reference namespaces, atomic publication, rollback and replay. All 39 profile schema/compiler/registry/CLI compatibility tests pass. Thirteen Intelligence UI tests cover assignment deletion, routing feedback, confirmation/focus and uncertain outcomes. Owner explicitly superseded the earlier dependency-blocking behavior on 2026-11-09.
