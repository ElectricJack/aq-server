# Actual Git producer-to-grant integration

Source `639f622` passed14 initial-grant tests, including two explicit actual-Git
cases, with no failures or ignores. Strict affected all-feature lint and workspace
formatting passed. [Retained evidence](evidence/aq08-actual-git-grant-639f622/artifact-sha256.json)
records exact daemon source, aq-git binary/source and system Git2.43.0 fingerprint.

The positive case uses an actual aq-git process to register/sync a disposable
repository, create a workspace checkpoint and retain its base. Native publication
exports that base, the authenticated source API serves both members in bounded
ranges, and actual Git verifies the downloaded bundle. Exact manifest/bundle
receipts agree with the grant owner's descriptors. Initial grant and replay retain
original versions, authority and hard deadline. An actual transfer-retention
release refuses grant while leaving preparation held. All new owner calls use
real wall time; no original timestamp or TTL is changed.

Graph, qualified runtime and material installation claims remain synthetic.
No physical worker hydration, harness/model launch, release, fleet throughput or
supported packaged-container Git profile is implied. This test joins the actual
Git export/retention owner to daemon admission. It exposed a separate review gap:
the prior grant implementation only checks original bounded Graph provenance.
Fresh pre-grant Graph observations are being added independently; this checkpoint
does not claim to detect task changes during the original observation lifetime.
