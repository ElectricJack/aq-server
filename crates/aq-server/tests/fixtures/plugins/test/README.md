# Plugin protocol fixture

This package registers the command `ext.test.ping` and event
`ext.test.pinged` using the [playbook plugin protocol](../../../../../../docs/specs/0018-plugins.md).
Run it on Linux with Python 3.11 or newer from the repository root.

```sh
python3 crates/aq-server/tests/fixtures/plugins/test/check.py
```

The check runs the real executable with the protocol's cleared environment,
vault secret path, project binding, writable state directory and a
1,000 ms child budget from its 2,000 ms command contract. It checks the
response and event payload, repeats a call byte for byte, and retries a crash
and a timeout with their original keys. Use `invoke()` as a starting point
for running another package, passing its executable, command, input,
project binding and expected output.

In AQ, the good event's dotted fields become subject `sample`, revision
`5` and correlation `{"ref":"ready","subject.id":"sample"}`.
AQ sets producer `ext.test` and derives the ID from the intent ID and the
event's zero-based position.

Change `input.mode` to exercise the response paths.

| Mode | Fixture behavior | AQ host outcome |
| --- | --- | --- |
| `normal` | Declared outcome and one declared event | Retains the result and event |
| `crash` | Saves the response and exits nonzero on the first call | Retries with the same key and retains the saved response |
| `timeout` | First call exceeds the child deadline | Terminates the child and retries with the same key |
| `nonzero`, `bad_json` | Exits nonzero or emits invalid JSON | Unknown until retried or expired |
| `wrong_outcome`, `wrong_output` | Exits zero with an undeclared outcome or invalid output | Retains `refused/invalid_plugin_response` and discards events |
| `invalid_event`, `host_event` | Appends a malformed event or a host event to a valid event list | Retains `refused/invalid_plugin_response` and discards the entire event list |

The standalone check exercises the transport. AQ's database retention,
notifications and trigger matching are host-side checks.
