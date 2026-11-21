# Supervisors read code (phase 2)

Date: 2026-11-20. Status: approved by the owner. Phase 2 of `2026-11-20-supervisor-full-access-design.md`.

## Intent

Supervisors can read every project's code, but cannot write it, so they can check facts themselves. Examples: a capability name, or an agent's unmerged work on its task branch. Authority follows the supervisor model:

- the global supervisor can read every project's repository;
- a project supervisor can read only its own project's repository.

## Approaches

- **Chosen: read-only fetch through the server, into a real checkout.** On ms1, aq-git's git-over-HTTP socket and the server's `[delivery] git_http_socket` are already live; the delivery pilot turned them on. aq-git serves a fetch as a plain Git server:
  - only `refs/heads/main` and `refs/heads/aq/*` are visible (`transfer.hideRefs`);
  - the grant headers `aq-base` and `aq-task-ref` are only checked for format, and they constrain pushes only.

  So no aq-git change is needed.
- *Rejected: a read-only mode in aq-git.* It needs an aq-git release and rollout, and buys nothing here.
- *Rejected: server-side "read file" commands.* They give the agent much weaker tools than a real checkout with git and ripgrep.

## Design

The whole feature needs both `supervisor_full_access` and `delivery_playbook`. ms1 builds with both.

1. **Server route** `GET|POST /v1/supervisor/git/{repository}/{*path}` on the machine listener, mounted with the other supervisor routes (`aq-supervisor-control` `machine_routes`).
   - **Caller:** a supervisor bearer with scope `owner`.
   - **Allowed requests:** only `GET info/refs?service=git-upload-pack` and `POST git-upload-pack`. Every other method, path or service, `receive-pack` included, gets 403 `Supervisors can only fetch`.
   - **Repository authority:**
     - Global supervisor: `{repository}` must be the `repository_id` of some project binding.
     - Project supervisor: it must be its own project's `repository_id`.
     - Otherwise 403 `Supervisor can read only its projects' repositories`.
   - **Forwarding:** to aq-git's git socket, the same way the node proxy does (`aq_delivery::git_grant::serve`). Use a fixed read grant:
     - `aq-base` = 40 zeros;
     - `aq-task-ref` = `refs/heads/aq/supervisor-read`;
     - `aq-max-push-bytes` = 1;
     - a fresh `aq-operation`.

     These only satisfy aq-git's header syntax; fetch ignores them, and no push is ever forwarded.
   - **Limits:** the transfer deadline is the Settings `delivery-limits.git_transfer_timeout_ms`.
   - **Admission:** the lane is the same one the node `/v1/git/` proxy uses.
2. **Supervisor host tool** `aq-supervisor-host repo sync PROJECT_ID`.
   - It resolves the project's name and `repository_id` with `project.show`, through the command endpoint as the other tools do.
   - It clones into `/workspace/repos/<project-name>`, or fetches if the clone exists. It checks out `main` and fetches `+refs/heads/aq/*:refs/remotes/origin/aq/*`.
   - Git authenticates with the turn's bearer, passed as a `GIT_CONFIG_*` environment entry for `http.extraHeader` on the git child process. It never goes in argv, a remote URL or a config file.
   - It prints the checkout path and the `main` head.
3. **Image.** The supervisor image gets `git` and `ripgrep`, because its root filesystem is read-only. This covers `Dockerfile.supervisor-host`'s final stage and the ms1 overlay recipe.
4. **Instructions** (flag on). Add: "To read a project's code, run `/usr/local/bin/aq-supervisor-host repo sync PROJECT_ID`, then read `/workspace/repos/<name>` with git and rg; task branches are `origin/aq/<task-id>`. Code access is read-only: never commit or push; delegate changes to worker tasks."

## Non-goals

- Any write: push, commit, PR or merge.
- Repositories that aren't project-bound.
- A change to aq-git.

## Constraints

- With `supervisor_full_access` or `delivery_playbook` off, nothing changes.
- Zero net lines in `crates/aq-server/src`, because the aq-server lib line budget is at its limit.
- Register the new route in `protocol/commands/v1/routes.json` as reviewed.

## Acceptance

- A global supervisor clones any bound project's repository and fetches a task branch.
- A project supervisor clones its own repository, and gets 403 for another project's repository.
- A `git push` through the route gets 403, and the aq-git refs are unchanged.
- A request without a bearer gets 401.
- Supervisor `repo sync` works from the supervisor image with no token in argv.
- On ms1, after rollout, the global supervisor reads a file from a repository it synced.
