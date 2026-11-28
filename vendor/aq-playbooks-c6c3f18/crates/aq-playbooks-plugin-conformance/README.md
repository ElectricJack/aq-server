# F5 plugin conformance driver

`aq-plugin-conformance` runs an installed plugin package on a Unix host without an AQ server or database. Use fixture credentials, a disposable vault and a disposable state directory. The package directory must have the name declared in its `plugin.toml`. The driver validates the manifest and catalog using the playbook model, launches the executable with the F5 environment, and checks each outcome, output and event envelope against the installed contracts.

```sh
cargo build --locked -p aq-playbooks-plugin-conformance
target/debug/aq-plugin-conformance \
  --plugin-dir "$PLUGIN_DIR" --vault "$FIXTURE_VAULT" \
  --state-root "$STATE_ROOT" --project "$PROJECT_ID" --fixtures first.json
```

The supplied project must be a UUID. By default the driver rereads that project's `[projects."<uuid>"]` table from the vault binding on each call. To inject a fixture binding directly, pass `--binding binding.json` with a JSON object. The manifest still determines the secret paths, which must be mode 0600 files inside the supplied vault.

## Fixture format

Each case has a unique `id`, a registered `command` and its `input`. Without `expect`, the case checks for any valid response and a byte-identical replay. Use explicit expectations to test which response or failure is returned.

```json
{
  "cases": [
    {
      "id": "initial-call",
      "command": "ext.test.start",
      "input": {},
      "expect": {
        "outcome": "started",
        "event_names": ["ext.test.started"]
      }
    }
  ]
}
```

Use `"failure": "timeout"` in `expect` to require that a call exceeds its child budget and produces no accepted response. Other failure kinds are `unavailable`, `nonzero_exit`, `io`, `stdout_limit`, `invalid_json` and `invalid_response`. Use `"retry_after": "timeout"` with success expectations to check an otherwise valid call that initially times out. The second attempt keeps the key and receives the smaller of half the command deadline and the remaining intent time. After a successful attempt, the driver checks exact stdout replay with a fresh validation budget. Stderr is drained within the host's capture bound and kept out of the JSON report.

The driver exits zero when every case meets its expectations. Each successful response is available under `cases[].result.response`, with validated event envelopes under `cases[].result.envelopes`. For example, a fixture using a response with `output.handle` can populate a later case in a script.

```sh
run_fixture() {
  target/debug/aq-plugin-conformance \
    --plugin-dir "$PLUGIN_DIR" --vault "$FIXTURE_VAULT" \
    --state-root "$STATE_ROOT" --project "$PROJECT_ID" --fixtures "$1"
}

run_fixture first.json > first.result.json
python3 - <<'PY' > second.json
import json
from pathlib import Path

data = json.loads(Path("first.result.json").read_text())
assert data["status"] == "passed"
response = data["cases"][0]["result"]["response"]
case = {
    "id": "follow-up",
    "command": "ext.test.follow",
    "input": {"handle": response["output"]["handle"]},
    "expect": {"outcome": "finished"},
}
print(json.dumps({"cases": [case]}))
PY
run_fixture second.json
```

Use a fresh state root and fresh case IDs for independent scenarios. Use fresh case IDs to force new calls when state is intentionally shared. Keep the project, state root and case ID stable when checking a replay across separate runs. The replay compares exact stdout bytes. It cannot show whether an external operation was repeated, and the JSON always reports `external_side_effects_verified: false`. Assert the operation count separately in the downstream fake executable or service.

Run the upstream fixture checks with `cargo test --locked -p aq-playbooks-plugin-conformance`. These checks use a local Python fixture and cover the process environment, timeout and process-group termination, exact replay, event derivation and rejection of invalid plugin responses.
