

## AQ delivery host contract for independent reviews

The AQ server extends the generic catalog 2.3.0 descriptions of `work.read` and `work.complete` for an independent review. The registry represents these facts with its existing string fields.

For a task designated by the `review-only` type or label, the server reports `facts.kind = review-only`. Designating a task alone establishes no verdict. For that task the server computes `facts.outcome` itself. The worker protocol cannot issue `review_approved`, `review_changes_requested`, or `review_incomplete` as process outcomes.

The first two values attest that the successful review result contains the corresponding verdict on the assignment's exact target report UUID, immutable report revision, and verified retained commit. The server matches the assignment version, requires a different task in the same project, and checks that the reviewer's source is unchanged and retained. It returns `review_incomplete` when the evidence is missing or fails verification.

With a verified review outcome, `facts.kind = review-only`, `facts.changed = false`, and a nonempty `facts.head`, `work.complete` may close the review assignment on its own result ID. Either verified verdict qualifies. The review publishes its verdict in the retained result and requires no merge. For implementation delivery the catalog's usual publication and merge instructions apply.
