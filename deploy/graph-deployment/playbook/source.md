# Deploy aq-graph to the development canary

An owner of the opted-in project submits the full merged aq-graph source
revision and the last observed desired generation. Use zero for a new
environment. Register the revision through the deployment extension, which
selects the immutable build approved for that revision.

Observe the returned generation until the controller sees a healthy Graph
container running the verified image, the deployment fails, or a newer
generation supersedes the request. Poll every ten seconds for at most sixteen
observations. After that, report the generation and observation timeout. The
controller can continue reconciling it.

Report a refusal with its reason and a deployment failure with its diagnostic.
On success report the generation, observed OCI manifest digest and provider
container ID. A later manual run can observe the same operation or retry a
failed current generation using its pinned build.
