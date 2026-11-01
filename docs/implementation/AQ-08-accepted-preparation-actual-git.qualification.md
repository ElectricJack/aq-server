# Actual retained Git reads after phase proposal expiry

Test source `0c74bf4` adds one explicit opt-in case. It passes against pinned
external aq-git SHA `c2ff26f5e63008aec0fbd4082a547e87825e651d4aba0744c4b9fdf752d58c39`
(source `2035654eeda57ded8880ea3f518ff0b4d324fa49`) and Git2.43.0. Final test4.80s,
focused strict all-feature lint1.51s, package format and whitespace pass.
[Machine evidence](AQ-08-accepted-preparation-actual-git-evidence/report.json)
retains exact source/log hashes, command, compile correction and sanitized trace.

The test accepts during the original proposal window, then waits using the real
clock. Source reads begin1234ms after proposal expiry, inside the independently
owned accepted lease. Original proposal/source clocks, source profile, base object
and transfer manifest stay unchanged. Actual retention inspection and bounded
manifest/bundle ranges succeed, exact digests match, and real Git verifies the
bundle. Exact accepted replay preserves its receipt. A structurally complete
phase Prepare refuses the legacy issuer. Explicit actual export retirement then
refuses reads while the accepted lease is still live; no Grant/installation
receipt is created and task ownership remains held.

The historical SourceAdmission itself had not expired during these reads. This
probe qualifies **proposal expiry**, not arbitrary future source availability or a
new fresh source-eligibility receipt. Graph/runtime/resource-journal/installation
qualification remains synthetic and reader-only; no physical worker hydration or
harness executes. Candidate030 is manually applied to disposable state28, with no
ordered migration or default activation claim. V2 fresh execution issuance,
worker phase adoption, renewal/closure, provider/notification calls and PB04/general
cold-preparation performance remain outside this evidence.
