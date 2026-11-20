# Run an epic feature retirement

Use the owner CLI to register a compile-time flag, approve its vetted revisions and close the epic after ordinary delivery and cleanup verification. The workflow uses AQ Graph v1 changesets, verified records and task dependencies. The delivery playbook can pick up the released cleanup task through its normal result-ready flow.

## Register staged tasks through the AQ owner CLI

Use `epic.retirement.register` on an AQ server that advertises it in `aq commands`. On the desktop, connect through the existing `--local` loopback owner session. For HTTPS, use the existing owner authentication with `--cf-access-token-file`. AQ binds the registration to the authenticated account's immutable principal ID and dispatches as its configured Graph service. The owner supplies no Graph token or actor name. The Graph service must already support verification. An underprivileged service causes the atomic Graph write to be refused.

The owner command adopts the two draft tasks in the registration manifest. They must have confirmed AQ task-creation receipts under the same owner and project, and approval must already block cleanup. Supply the epic's current version as `expected_version`. For example, after this command is available, the accountable desktop owner can run the following with the prepared manifest and current epic version. Assign a fresh UUID to `operation_id` and retain it for any replay.

```bash
python3 -c 'import json, sys; p = json.load(open(sys.argv[1])); p["expected_version"] = sys.argv[2]; print(json.dumps(p))' \
  "$registration_manifest" "$epic_version" |
  aq --local --origin http://127.0.0.1:28080 --json run epic.retirement.register \
    --operation-id "$operation_id" --params-file -
```

Use `aq --local --origin http://127.0.0.1:28080 --json operations show "$operation_id"` to recover the operation and Graph receipt. The registration records the original task IDs and adds a protected closure guard. It preserves the draft approval at its previous revision and leaves the protected cleanup draft with the original blocker. This command exposes registration only. Decisions and closure for an AQ-owned registration require a separately authorized AQ action.

## Run the direct Graph qualification workflow

Run the direct CLI against a disposable Graph workspace or with a separately provisioned Graph owner account. Use `aq projects show PROJECT_ID` to confirm the project binding. The epic must already exist as an open task in that workspace. Keep the legitimate Graph credential in a private token file.

```bash
retirement() {
  python3 scripts/epic_feature_retirement.py \
    --graph-origin "$GRAPH_ORIGIN" --workspace "$GRAPH_WORKSPACE" \
    --owner "$GRAPH_OWNER_ACTOR" --token-file "$GRAPH_OWNER_TOKEN_FILE" "$@"
}
```

Use an HTTPS Graph origin or a loopback HTTP listener. The owner is the actor printed by Graph for its own changesets, rather than an arbitrary name supplied inside a decision. The CLI checks the proposal's authenticated actor against the owner before applying each batch. Graph enforces administrative authority for verification and protected task transitions.

## Register and release cleanup

1. Run `retirement register registration.json` with the following manifest. Keep the returned registration ID and attach it and the cleanup task ID to the flag-introducing PR.

   ```json
   {
     "epic_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
     "project_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
     "generation": 1,
     "repositories": {
       "Example/service": "feature_a",
       "Example/client": "VITE_FEATURE_A"
     },
     "ordinary_default": false,
     "permanent_behavior": "Enable feature A in ordinary builds after retirement."
   }
   ```

2. Registration first records the policy and then ensures its dependencies. Repeating `register` with the original manifest or running `retirement ensure REGISTRATION` reconciles a partial setup. One registration per workspace, project, epic and generation determines the approval and cleanup IDs. To adopt two already-filed tasks, supply `"adopt": {"approval": "APPROVAL_TASK_UUID", "cleanup": "CLEANUP_TASK_UUID"}` on the initial registration. Both must be the owner's draft tasks with no conflicting links. Adoption preserves their IDs and stays blocked. Use `retirement amend REGISTRATION additions.json` with `{"repositories":{"Example/new-repo":"feature_a"}}` to add affected repositories before approval.
3. Vet the exact participating revisions and retain the integrated test report. Run `retirement decide REGISTRATION decision.json`. The first decision revision is 1. Advance the decision revision for each new decision, including when requesting changes. Use full Git commit SHAs.

   ```json
   {
     "revision": 1,
     "choice": "approve",
     "vetted": {"Example/service": "1111111111111111111111111111111111111111",
                "Example/client": "2222222222222222222222222222222222222222"},
     "evidence": {"artifact_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                  "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}
   }
   ```

   Use `request_changes` to leave the approval and cleanup draft. Approval records the verified decision, completes the protected prerequisite and activates the existing cleanup task in one Graph batch. The protected draft closure task keeps the epic open even if the cleanup agent's result settles early. All three tasks have deterministic IDs, or the two explicitly adopted IDs.

## Record evidence and close

Use `retirement evidence REGISTRATION evidence.json` for each receipt below. Every manifest includes `kind`, `approval_revision` and `evidence`, an object with the retained `artifact_id` and its content `sha256` as in the decision manifest. These are owner attestations about artifacts inspected by the owner. Graph records the provenance and owner verification. Preserve the referenced bytes separately and confirm they match the cited digests before recording them.

| Kind | Additional required fields | What the owner verifies |
| --- | --- | --- |
| `merge` | `repository`, `pull_request` (HTTPS URL), `sha` | The registered repository's cleanup PR merged at that full SHA. Preserve the forge and AQ delivery receipts when available. |
| `ordinary` | `repository`, `sha`, `passed`, `flag_absent` | At the recorded merge SHA, the normal build passed without the flag and the source, manifests and active workflows no longer reference it. |
| `integrated` | `revisions` (repository to merge SHA), `builds` (repository to the exact passing ordinary-build `evidence` object), `passed` | The integrated test used those exact ordinary builds. It includes the other active epics' required behavior. |

A failed or unmerged PR stays in the normal delivery repair flow. Record its merge receipt after the corrected PR merges. Retain failed ordinary or integrated checks with `passed: false`, then record a fresh report when repaired. If a repair required another merge, add `supersedes_merge` with the repository's current ID from `status.merge_receipts` to the repair merge manifest. That retains the old receipts and requires a new ordinary build for the repaired SHA and a fresh integrated check. Run `retirement status REGISTRATION` to see the active decision, blocked repositories, failed checks and open work. Once the cleanup task has completed through normal AQ work settlement and all evidence is present, run `retirement close REGISTRATION`. Graph completes the protected closure task and the epic atomically with a verified closure receipt. Graph also checks ordinary child-task completion.

The JSON output for a mutation is the applied Graph changeset receipt, including `change_set_id`. Registration returns both the registration and setup receipts. Repeating an applied operation recovers its original receipt from Graph history. Each new attempt freezes expected versions and checks the author before applying. After a transport timeout, repeat the same command and manifest. On a concurrent edit, reread with `status` and reconcile the current facts.

## Qualification boundary and follow-up

- This handoff uses an owner-operated Graph API workflow. Graph administrators retain their ordinary ability to edit protected state outside the workflow. A dedicated AQ decision grant, dashboard decision inbox and automated playbook admission for an epic-level approval would need separate authority and presentation work.
- CI inventory, forge merge and Graph settlement receipt discovery remain operator inputs. The CLI checks repository identities, evidence revisions, build artifact references, Graph task state and recorded decisions. It does not contact the forge, verify artifact contents or derive a repository's active flag matrix on its own.
- Vetted revisions are pinned by the decision. Unrelated movement on main leaves the decision intact. For a material change before approval, submit a request for changes and vet a new revision. After release, this workflow refuses to rewrite that approval. The owner must stop any affected work through AQ and arrange a new vetting cycle before relying on the old cleanup. Safe reblocking of in-flight work needs an AQ execution fence alongside the Graph state.
- Run `python3 scripts/test-epic-feature-retirement.py` with `rustc` and the disposable Graph and PostgreSQL images named in the test. The acceptance covers partial registration, two independently compiled flags, owner-only release, receipt replay across restarts, one failed and repaired two-repository cleanup sequence, and adoption of already-blocked draft tasks. The test compiles and runs the scratch binaries. Its recorded forge and artifact identities are fixtures.
