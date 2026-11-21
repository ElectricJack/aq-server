# Enroll a repository in automatic delivery

One AQ project delivers a code task through one branch, `refs/heads/aq/<task-id>`, and one pull request into the repository's default branch. Start enrollment with a single task. Widen it after AQ has recorded the merge and a dependent task has started from a commit containing that merge. [Delivery specification](../specs/0019-integrated-delivery.md)

## Prepare GitHub

1. Give the existing AQ GitHub App installation access to the repository. Verify its canonical owner/name, numeric repository ID, default branch and permitted merge methods with `gh api 'repos/{owner}/{repo}'` from the target checkout. AQ needs to create and update pull requests, read checks and branch rules, and write contents. An installation token scoped to the repository should be able to read its metadata and applicable rules.
2. Inspect a representative pull request's checks on its **head SHA**. Choose jobs that run on every eligible PR, including documentation-only changes. Release jobs triggered only by tags or pushes to the default branch cannot gate that PR. Run the jobs successfully before requiring them.
3. Require those checks on the default branch, pin the producer of each check, and require the tested branch to be current. Verify the effective rules using `gh api 'repos/{owner}/{repo}/rules/branches/main'` (substitute the default branch). Include classic branch protection in the check when present. Keep AQ subject to the same rules. For `aq-graph`, the pilot requires GitHub Actions jobs `check` and `deployment`. The latter exercises a disposable recovery drill.

## Prepare central Git and AQ

1. Configure one central aq-git credential profile for the exact repository HTTPS URL. Mint renewable, single-repository App tokens for central Git. Its profile needs contents-write permission. Mount the App key only in the trusted components that mint tokens. The server performs PR operations with the App. Workers access central Git with their own attempt grants. [Central Git credential setup](https://github.com/ElectricJack/aq-git/blob/main/deploy/github-app/README.md)
2. Verify that the AQ project resolves to the same repository, binding generation and default branch. An existing seeded project may predate repository onboarding. Check its actual central Git origin, including for a public repository where anonymous reads succeed.
3. Verify that the binding permits the task-branch prefix. Run `scripts/add-task-ref-prefix.py PROJECT_ID` to inspect an existing binding. Its apply mode performs a drained generation change. Active transfers, checkpoints and candidates can belong to historical tasks. A refusal leaves that history in place. Resolve the migration strategy for those handles before modifying the live binding.
4. Select the named default branch as the source of new work. `scripts/bind-project.py PROJECT_ID --branch refs/heads/main` describes the change. Its apply mode syncs and retains a commit and emits the configuration entry. Preserve existing placement and priority when updating the entry. Increment the profiled execution revision when its content changes.
5. For a seeded project, record the verified delivery destination through the owner endpoint `POST /api/repositories/adopt`. Pin the operation ID, project, repository UUID, binding generation and canonical GitHub remote. AQ checks the retained base and forge identity. Save the receipt and reuse the operation ID on retry. PR-only projects grant writes to `refs/heads/aq/` and keep the default branch protected. The [seeded-project adoption guide](../implementation/D2.md#adopting-a-seeded-project) includes the request and CLI.
6. Use a server image that includes delivery (every ordinary build since the Epic 0 flag was retired), and install a compatible worker, its matching sandbox helpers and central aq-git with smart HTTP enabled. Verify the private Git socket, lookup of an enrolled attempt's grant, and the legacy result path on the same worker. Record a restorable backup before upgrading the live database and rehearse the upgrade on a separate copy.

## Verify task commit attribution

For a multi-user installation, use individual AQ logins. Tasks created with a shared desktop login carry the shared account's email. Git stores this address in the commit, including when the repository is public.

Upgrade worker-side aq-git and the workers before enabling the server that sends task authors. Create two tasks in the same repository from different AQ accounts. Compare the **Requested by** field on each task with the author of its task-branch commit. Repeat after returning one task for repair. Inspect the task commit SHA with:

```sh
git show -s --format='author    %an <%ae>%ncommitter %cn <%ce>' <task-commit-sha>
```

The author email should match the task creator's AQ account, and the committer should identify the worker. Check the PR's merge actor separately. Both tasks use the same repository binding.

## Activate and prove one task

Qualify the routing and delivery policies together against the target catalog. Each namespace pins its catalog. Preserve the old routing artifact, fixtures, approved capabilities and activation record when opening a new namespace. Verify that an ordinary routing task still reaches its intended level, and that a task outside enrollment still settles.

Scope the pilot policy and its host enrollment to the same project and task IDs. Verify the sealed artifact digest, its capability grant, the grant's lifetime and its active binding before making the task ready. A grant issued for a short rehearsal needs an enduring owner-approved replacement before unattended operation.

In the server vault, add the project UUID and first task UUID to the delivery configuration. Supply the actual private socket shared with central aq-git.

```toml
[delivery]
git_http_socket = "/run/aq-git/git-http.sock"
project_allowlist = ["<project UUID>"]
task_allowlist = ["<first task UUID>"]
```

The lists intersect when both are present. An empty project list disables new delivery attempts installation-wide. An empty task list also disables new delivery attempts. Omitting the task list opts listed projects into delivery for all their new tasks. An attempt keeps the route recorded with its grant through worker restart and subsequent enrollment changes.

Watch AQ publish the task branch, open or recover the PR, observe all required checks on the candidate SHA and merge under branch protection. Compare GitHub's actual merge commit to AQ's persisted merge evidence. Verify the remote and central task refs are gone, and Graph has acknowledged completion. A manual merge cannot exercise these steps. For the first dependent task, verify that the retained checkout includes the recorded merge. Exercise repair on a failed check and reconciliation after a lost merge reply on an isolated stack.

Expand the project enrollment only after these checks pass. When pausing a rollout, stop handing new attempts to the delivery path and reconcile the attempts that already hold grants. Preserve their PRs, merge evidence and routing binding during recovery.

## Enable a development environment separately

Opt in each project's named deployment target and its approval policy. Use the recorded merge SHA to select a build, retain its immutable digest, dispatch the existing release mechanism and verify the running environment reports that revision or digest and passes readiness. Reconcile a delayed or repeated dispatch against the latest desired generation. Keep merge completion visible while a deployment waits or fails. Production promotion follows its own approval rule.
