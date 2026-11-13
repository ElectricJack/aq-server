# 0012 Internal work: implementation notes

Notes on how the build differs from [the spec](../specs/0012-internal-work.md)
or adds to it, for the spec's owner.

## Logged-in Codex runs (`aq-supervisor-host internal`)

- **Deviation:** the Codex CLI has no switch that removes its shell tool. "Tools off" for Codex is a read-only sandbox in an empty directory with shell environment `inherit="none"`.
- A login file is copied in and back under an exclusive `flock` taken on a read-only descriptor of the login file's directory; nothing is created there. The directory needs to be writable only for a refresh to be written back, so a read-only mount serves every run that does not refresh the login. The copy-back is a compare-and-swap: a refreshed login is written back only if the file still holds the bytes the run copied in. Each internal work host should be given its own copy of each login file; sharing one file between hosts relies on the compare-and-swap above and loses a refresh when two hosts refresh at once.

## Logged-in Claude runs (`aq-supervisor-host internal`)

- **Tools off:** `--tools ""`, which the CLI help lists as "Use \"\" to disable all tools". Read from the help of Claude Code 2.1.323 (the CLI installed on the development machine) and checked to be the same in 2.1.319. The worker pin is 2.1.318, which is not installed locally; its help was not read. No `permissions.deny` list is written.
- The run is `claude -p --output-format json --input-format text --model <model> --setting-sources user --settings <home>/settings.json --strict-mcp-config --no-session-persistence --disable-slash-commands --tools ""`, with the prompt on stdin and a fresh empty working directory. The generated `settings.json` holds only `disableAllHooks: true` and `enableAllProjectMcpServers: false`.
- The setup token in `secret_file` is read with the owner-only protected read, normalised (one line of printable ASCII, trailing newline dropped) and passed only as `CLAUDE_CODE_OAUTH_TOKEN`. It is never written to disk, so a Claude login needs no lock and nothing is copied back. Every text that leaves the run (the answer and a failure detail) has the token replaced first.
- The answer is the `result` string of the one JSON object on stdout; `usage.input_tokens` and `usage.output_tokens` fill the usage. `is_error` other than `false` or a non-zero exit fails the candidate: `provider_unavailable` when the `result` text names a login or usage-limit problem, `provider_error` otherwise. Stdout that is not a result object is `provider_error`, with the redacted output as the detail (at most 500 bytes).
- The login and usage-limit wording now also matches Claude Code's ("limit reached", "hit your limit", "OAuth token", "Invalid API key"). These are taken from the CLI's known messages, not from a live failing run.
- A host claims tasks for each harness it has configured. It still runs one task at a time, so a Codex and a Claude task on the same host run one after the other.
