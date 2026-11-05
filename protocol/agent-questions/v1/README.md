# Agent questions v1

These are the agent-facing question routes served by `aq-server` (F5). An agent asks its owner one question and waits for the answer. `routes.json` lists the routes, their bodies and their error codes. The unit test `question_routes_match_contract` checks the route table against it.

## Rules

- **Authentication:** the per-attempt machine credential. The node and attempt come from the authenticated binding.
- **Ask** (`POST /v1/agent/questions`): creates a typed `question` decision bound to the task, attempt and session. The subject is the task, and `subject_revision` is `attempt:{attempt_id}:{generation}`. `question_digest` is the digest of the question bytes. The same transaction registers an H4 `human_decision` wait (`wait_id` in the response), so the decision and the wait exist together or not at all. Because there is one active wait per attempt, asking while another wait is active gets `wait_conflict`. The F4 `question.asked` notification carries only the decision ID, never the question text.
- **Replay:** the same `operation_id` with the same question returns the same decision. A different question under the same `operation_id` gets `id_conflict`.
- **Show** (`POST /v1/agent/questions/show`): the caller task's own decision, with `answer` and `receipt_digest` once it has been answered.
- **Answer:** only the owner answers, through the G1b action `question.answer` (dashboard inbox or an authenticated link). The answer targets the exact `decision_revision`. A stale revision is a conflict and changes nothing. Replaying the same owner operation with the same text returns the same receipt; any other second answer is a conflict. Discord replies and notification acknowledgements are never answers.
- **Delivery:**
  - while the asking attempt is live (still holding the task, not cancelled, binding not revoked), the answer wakes it exactly once through the wait (`delivery.kind = "wait"`);
  - once the attempt has ended, the answer is queued as task-addressed mail on thread `decision_id` from the system component `question_answer`, for the continuation (`delivery.kind = "task_mail"`). Session-addressed mail is never retargeted.
