# Provider reset deployment restored, 2026-11-12

A subsequent Providers settings deployment replaced the first reset-details
release with server and dashboard revisions that omitted the reset changes. The
live API consequently lost `reset_credits`, leaving only weekly reset times.

The combined release preserves Providers settings and schema 118 and restores
reset collection, contracts, and display. Server code is
`7ac18941d3ed3923dffdd0b3d5e2217c0ce56f94`; dashboard bundle source is
`971231df43f12e7702a0333e7f8827d88181b3db`.

The deployed server image is `agent-q-server:ms1-providers-with-resets-20261112`,
image ID `sha256:7205aa3e13cc9fe95f4f2119d41cec82704edf8339a327ddea4bd848765f9e13`.
The official dashboard manifest digest is
`sha256:596c98b219c17cdb761fcb59c6565993f24ac7b6698a4521acd52bbc3594b3d3`.
The clean active `feat/swarm-concurrency-controls` worktrees were fast-forwarded
to include both implementations, so their next build retains reset details.

Root independently passed 97 selected dashboard tests covering Providers,
Intelligence, settings navigation, reset adapters/components and polling. Terra
also passed 110 focused tests, type checking, lint, schema checks and the official
production build. Root verified the live bootstrap digest and all 65 served asset
hashes, real API data, dropdown behavior at 1440/390/320 pixels, Escape focus
restoration, no horizontal overflow, and the existing Providers and Intelligence
pages. Root opened the live desktop/mobile reset screenshots and Providers page.
No browser errors or mutating browser requests occurred.

The live dropdown now displays **Available resets: 0** for Codex and
**Available resets: Unknown** for Claude, independently of weekly reset times.
Claude's supported structured usage control still cannot establish an independent
reset grant count; Unknown remains intentional.

Provider settings, hierarchy, profiles and limits were compared before/after and
preserved. The worker/supervisor/dependency containers were unchanged, schema
remained 118, and the existing immutable usage producer timer was retained.
Only the server container and dashboard bundle were replaced.

Private local deployment receipts, configuration backup, asset verification and
live screenshots are retained at:
`/home/jkern/.local/share/aq-ms1/provider-reset-combined-rollout-20261112T184945Z/`.
This deployment record supersedes the earlier implementation report's local-only
operational status. No commits were pushed and main was not changed.
