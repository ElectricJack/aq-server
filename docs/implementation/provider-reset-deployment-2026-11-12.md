# Provider reset preview deployment, 2026-11-12

The user authorized deployment for testing. The live AQ MS1 preview now runs
server source `9f0efa681e12e7b0b66b8a8d567f955c6eb8ca98`, image
`agent-q-server:ms1-provider-resets-20261112`, with state format 117. The image
was built from an isolated clean worktree using the repository Dockerfile and
`ci-image` profile. It stacks the reset changes on the previously deployed
Intelligence hierarchy server `ea29ae95`; no migration or schema downgrade was
introduced. Ongoing uncommitted provider-settings work was untouched.

The official dashboard source is
`5c56c7f7c99410ec10f1b574771b02822896c85c`, stacking the reset UI on the previously
deployed compact Intelligence editor `f333867`. Its 65 served assets match the
manifest byte-for-byte; the manifest digest is
`sha256:bdea2ce2fcbeb0bc687d826845049e248b3fdd0fa1b957400d9ba840344d5973`.

The host usage timer now invokes an immutable release copy of the reviewed
producer scripts. Both account reads succeed. Live Codex reset availability is
known zero; Claude remains explicitly unknown through the selected SDK control,
while its real quota windows continue to refresh. No reset credit was consumed.

Before activation, root retained the previous Compose override, usage service,
container identities, settings reads and a private PostgreSQL dump. Only the
server container was recreated with `--no-deps`; other containers, profiles,
hierarchy/routing Markdown and concurrency limits were preserved. The timer is
active and the server dependency healthcheck passes. The two previously online
worker records remain online; a pre-existing offline/network-retry worker is
unchanged and outside this deployment.

Root independently passed 10 producer tests and 89 focused reset/settings tests
on the combined source. The Terra dashboard integration also passed 105 focused
tests, typecheck, lint, schema validation and the official production build.
The full server image compiled successfully and the staged producer was checked
before activation.

Live read-only browser verification used the actual gateway at
`http://127.0.0.1:28444`. Root captured and opened the usage disclosure at 1440,
390 and 320 pixels and checked the preserved settings editor. The count and
unknown state are visible, the close control is usable, Escape restores focus,
the page has no horizontal overflow, and no browser page errors or mutation
requests occurred. Overview also returned successfully after startup.

Private rollback material, deployment receipt, preservation comparisons and live
screenshots are retained at:

`/home/jkern/.local/share/aq-ms1/provider-reset-rollout-20261112T182205Z`

Database data was not restored or modified for rollback. The previous server
image is also state-format-117-compatible. This is the local owner preview;
no remote release or main-branch publication is claimed.
