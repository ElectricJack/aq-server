# Native reconciliation qualification

Source `3240739` combines reconciliation `1bf6a5b`, style correction `dd1fbd8` and the separately qualified aq-admission0.1.8 closure contract `f5f61b3`. The closure contract changes no reconciliation wire bytes and enables no closure owner.

All **51 tests passed, zero failed or ignored**, across native reconciliation, initial grant, expiry, offers, preparation and NeverGranted. The initial grant cases include the pinned actual aq-git binary. Strict affected-target all-feature Clippy and workspace formatting passed. Exact logs and source hashes are retained in the adjacent JSON and evidence directory.

Reconciliation covers current authentication after credential rotation under a real node lock; machine-only routing and bounded transport; exact owned and historical-boot scopes; offered/preparing/granted/expired and actual terminal receipt projections; conservative refusal with lost history; cursor progress; and unchanged hot state, journals and receipt counts. A granted projection contains no DeliveryClock and cannot authorize resumption.

Development failures are retained: invalid enum fixture, missing human-router CSRF header, a test attempting an immutable native-registration edit (replaced by genuine old-claim closure/new registration), and one Clippy conditional. Earlier passing runs predate the final combined source; only the 018 final logs establish the combined result.

Tests use actual PostgreSQL, HTTP routing and selected actual Git operations. Graph responses, qualification and material installation claims remain fixtures. No live worker driver, host process, provider or complete native execution is qualified here.
