# Playbook plugins

AQ server loads operator-installed extensions to the playbook catalog. An
extension registers project-scoped commands and events under its own namespace
and executes commands through the `aq.plugin/1` process protocol.

## Installation

Mount each package in `server.plugins_dir`, which defaults to
`/opt/aq/plugins`. Set `server.state_root` to the writable state volume, which
defaults to `/var/lib/aq-plugins`. Mount the package read-only and restart
AQ server to discover it.

```text
<plugins_dir>/<name>/
  plugin.toml
  contracts/registry.json
  contracts/notes.json
  bin/<exe>
<state_root>/<name>/
  tmp/
<vault>/plugins/<name>/
  bindings.toml
  <secret files>
```

The executable runs in the AQ server image. It must work with the image's
read-only root filesystem. AQ creates the writable state and temporary
directories for each loaded plugin. The vault holds operator-managed
configuration and secrets in files with mode `0600`.

```toml
name = "example"
version = "0.1.0"
protocol = "aq.plugin/1"
exe = "bin/example"
contracts = "contracts/registry.json"
notes = "contracts/notes.json"
secrets = ["plugins/example/token"]
bindings = "plugins/example/bindings.toml"
```

The package directory and `name` must agree and match
`[a-z][a-z0-9_]{1,31}`. The executable, registry and notes paths are relative
to the package. Secret paths and the optional bindings path are relative to
the vault. AQ logs refusals for invalid packages and continues startup.
The installed version is recorded in startup logs for provenance.

The registry uses the existing `aq_playbooks_model::Registry` JSON format.
The notes use the same format as the host catalog's `notes.json`. Every
command and event name begins with `ext.<name>.`. A command declares
`recovery: keyed_idempotent`, scope `project`, capability `ext.<name>`, and a
required `refused {reason: string}` outcome with open reason values. The
reason string must admit `invalid_plugin_response` and `plugin_unavailable`.
An event declares the trusted producer `ext.<name>`, project scope,
`subject_field`, `revision_field` and `correlation_fields`.

AQ keeps each namespace's immutable base profile and overlays the installed
extensions. Owners can issue a project grant carrying `ext.<name>` while the
plugin is installed. Artifacts pin the contracts they use by name and
fingerprint. Fingerprints depend on the contract and stay stable across plugin
package version changes. Artifacts that use the base catalog retain their
existing sealing behavior. Activation requires matching pinned extension
contracts. When a plugin is removed or its contracts change, the next affected
call in an in-flight run returns `refused/plugin_unavailable`.

## Invocation

AQ starts one `<exe> invoke` process per call with the plugin's state directory
as its working directory. It gives the process precisely these environment
variables.

| Variable | Value |
| --- | --- |
| `PATH` | `/usr/local/bin:/usr/bin:/bin` |
| `HOME` | Plugin state directory |
| `TMPDIR` | `<state_dir>/tmp` |
| `AQ_PLUGIN_PROTOCOL` | `aq.plugin/1` |

The process reads one JSON object from stdin.

```json
{
  "protocol": "aq.plugin/1",
  "command": "ext.example.sync",
  "input": {},
  "idempotency_key": "<stable across redispatch of this intent>",
  "project_id": "<AQ project UUID>",
  "binding": {},
  "state_dir": "/var/lib/aq-plugins/example",
  "secret_paths": {"plugins/example/token": "/vault/plugins/example/token"},
  "deadline_ms": 7500
}
```

AQ rereads `bindings.toml` on every invocation and passes the table under
`projects.<project_id>`, including nested tables, or an empty object when
the file or project table is absent. Operators can edit these bindings without
repackaging or restarting. The process receives paths to the secret files.

The child budget is half the command contract's `deadline_ms`, capped by
the remaining intent time. The request carries the actual budget. AQ
terminates the process if it reaches that budget. A nonzero exit, timeout or
unparseable stdout leaves the outcome unknown. AQ redispatches while the
intent deadline remains open, using the same idempotency key. AQ captures up
to 64 KiB of stderr and logs it with the call identity. Dispatch blocks the
namespace's engine thread for the duration of the call. Interactive command
deadlines should be 10 to 15 seconds. Set outbound timeouts inside the plugin
and return a declared retryable outcome when possible.

On success, stdout carries one JSON object of up to 1 MiB.

```json
{
  "outcome": "completed",
  "output": {},
  "events": [{"name": "ext.example.synced", "payload": {}}]
}
```

Schema string length limits count UTF-8 bytes. Strings bound for
`message.send` may contain newline and tab among control characters.

## Retention and replay

For a repeated idempotency key, return a byte-identical response including
all events, with no repeated side effects. Store the response before writing
stdout. Document recovery steps for operators if plugin state can advance
ahead of AQ.

AQ validates the entire response. The event contract names the payload fields
used for the subject string, nonnegative integer revision and string
correlation values. Use a dotted path such as `subject.id` for a nested property. Each retained event envelope is bounded to 64 KiB. AQ derives
`event_id` from the digest of the intent ID and zero-based event index. The
producer is `ext.<name>` for every version of the package. Plugins can only
produce events declared in their own extension.

AQ commits the command result and all validated events in the same
transaction. Events go through the existing trigger matcher and use the
contract's retention. An invalid outcome, output or event causes AQ to retain
`refused {reason: "invalid_plugin_response"}` and discard the entire event
list. AQ queues at most one notification to the owner per plugin per UTC day
using identity `plugin:<name>:invalid:<UTC date>`. A plugin that was refused
at startup is unavailable.

## Trust model

Plugins are trusted code that an operator installs. They run as the aq-server uid and can technically read anything aq-server can, including the vault. Clearing the environment prevents accidental leaks, not deliberate ones. Untrusted plugins need a future transport, such as a sidecar or a separate uid; this protocol allows that without changing the messages.
