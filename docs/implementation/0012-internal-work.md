# 0012 Internal work: implementation notes

Notes on how the build differs from [the spec](../specs/0012-internal-work.md)
or adds to it, for the spec's owner.

## Logged-in Codex runs (`aq-supervisor-host internal`)

- **Deviation:** the Codex CLI has no switch that removes its shell tool. "Tools off" for Codex is a read-only sandbox in an empty directory with shell environment `inherit="none"`.
- A login file is copied in and back under an exclusive `flock` on `<secret_file>.lock`, and the copy-back is a compare-and-swap: a refreshed login is written back only if the file still holds the bytes the run copied in. Each internal work host should be given its own copy of each login file; sharing one file between hosts relies on the compare-and-swap above and loses a refresh when two hosts refresh at once.
