# Lost grant response: actual process qualification

Source 36e2bba with worker ed21fbe passed three pinned-binary tests: lost response/history lookup (5.85 s), physical Git/tmux regression (6.44 s), and preparation after proposal expiry (6.94 s). A real daemon commits the grant, then test middleware returns HTTP 503. The separate worker retrieves the exact historical identity and remains Prepared at revision 26 without execution authority or a host. Exactly one prepare request and one history request occur. Claims remain held.

Affected-target strict Clippy passed in 30.37 s; format check passed. Source/binary hashes, commands and raw logs are retained in qualification.json. Package clean waited on another build before obtaining its lock.

This qualifies a test-only route and synthetic provider/registry/Graph configuration. The physical regression uses real Git and tmux with a synthetic harness. It does not enable real providers, remote terminal acknowledgements, renewals, releases or dashboard terminal controls. No deadline policy changed; schema remains 32.
