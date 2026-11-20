# Answer a host playbook decision with curl

Use the admin origin and authenticate as an installation owner. Only an owner
listed in the decision's `responders` can answer it. Use the
UUID in `handle` for the detail and answer paths.

For local development with dev authentication enabled, set `AQ_BASE` to the
admin origin and use your configured owner email.

```sh
curl --fail-with-body -sS -H 'X-AQ-Dev-Email: owner@localhost' \
  "$AQ_BASE/api/playbooks/decisions"

curl --fail-with-body -sS -H 'X-AQ-Dev-Email: owner@localhost' \
  "$AQ_BASE/api/playbooks/decisions/$AQ_DECISION_HANDLE"

curl --fail-with-body -sS -H 'X-AQ-Dev-Email: owner@localhost' \
  -H 'X-AQ-Request: 1' -H 'Content-Type: application/json' \
  --data '{"choice":"rejected","note":"Add a worked example for the recovery path."}' \
  "$AQ_BASE/api/playbooks/decisions/$AQ_DECISION_HANDLE/answer"
```

On a hosted installation, authenticate through its configured identity
provider. For Cloudflare Access, use an authenticated owner's Access
session cookie in place of the dev header. Set `AQ_DECISION_HANDLE` to an
open decision from the list and use one of its offered `choices`.
The expiry is expressed in Unix milliseconds.

The note is optional and may be up to 2,048 UTF-8 bytes. A note with text is
also posted as a comment when the subject is a task. A successful response
returns the retained `outcome` and `output`. Repeating an answer returns
the original completion. The server returns HTTP 403 when the owner isn't
listed as a responder and HTTP 409 when the decision has expired.
