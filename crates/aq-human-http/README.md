# aq-human-http

Shared human CLI HTTP helper, not a node client. `Client::new(origin,
Authentication::CfAccessTokenFile(absolute_path), Limits)` loads one private
Cloudflare Access JWT; credentials must be owner-only, regular, single-link,
non-symlink files. No token is included in Debug/errors. Production origins
require HTTPS. Redirects, environment proxies and automatic retries are disabled.

`get_json::<Response>("/api/...", query_pairs)` and
`post_json::<Request, Response>("/api/...", &request)` use fixed same-origin API
paths and bounded JSON. Mutations send `Origin` and `X-AQ-Request: 1`. Response
sizes and total/connect deadlines are finite; status errors never echo bodies.
Transport errors do not prove non-delivery. Callers retain stable operation IDs
and reconcile uncertain mutations explicitly; this helper never retries them.

The `loopback-development` feature exposes `LoopbackDevelopment`, which uses the
development server's configured identity, and `LoopbackDevelopmentEmail`, which
sends `x-aq-dev-email`. Both require an HTTP loopback origin and explicit selection
by the calling client.
`localhost` development connections are pinned to loopback addresses, and no
cookies or node credentials are used. Normal tests use disposable loopback fake
HTTP servers and temporary synthetic credential files.
