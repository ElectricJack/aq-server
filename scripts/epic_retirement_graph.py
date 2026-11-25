"""Owner-authenticated Graph reads, proposals and retrieval of committed receipts."""

import json
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

from epic_retirement_records import (
    RetirementError,
    canonical,
    require,
    uuid,
)


class GraphError(RetirementError):
    def __init__(self, status, payload):
        self.status = status
        super().__init__(f"Graph HTTP {status}: {payload}")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        """Refuse redirects for every authenticated request."""
        return None


def graph_origin(value):
    parts = urlsplit(value)
    loopback = parts.hostname in ("127.0.0.1", "::1", "localhost")
    require(
        parts.hostname
        and not any((parts.username, parts.password, parts.query, parts.fragment))
        and (parts.scheme == "https" or (parts.scheme == "http" and loopback))
        and parts.path in ("", "/"),
        "Use an HTTPS or loopback Graph origin",
    )
    return value.rstrip("/")


def require_retryable(reply, attempt):
    if reply.code != 503 or attempt == 2:
        raise GraphError(reply.code, json.load(reply)) from None


class Graph:
    def __init__(self, origin, workspace, token):
        self.workspace = uuid(workspace)
        self.base = graph_origin(origin) + f"/v1/workspaces/{self.workspace}"
        self.token = token
        self.http = build_opener(ProxyHandler({}), NoRedirect())

    def request(self, path, body=None, *, missing=False):
        request = self.prepare_request(path, body)
        for attempt in range(3):
            try:
                with self.http.open(request, timeout=10) as reply:
                    return json.load(reply)
            except HTTPError as reply:
                if reply.code == 404 and missing:
                    return None
                require_retryable(reply, attempt)
            except (URLError, TimeoutError, ConnectionResetError) as error:
                if attempt == 2:
                    raise RetirementError(
                        "Graph response unavailable. Retry the same command to recover."
                    ) from error
            time.sleep(0.25 * (attempt + 1))

    def prepare_request(self, path, body):
        return Request(
            self.base + path,
            data=None if body is None else canonical(body),
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
                "X-AQ-Graph-API-Version": "1",
            },
        )

    def node(self, node_id, *, missing=False, kind=None):
        node = self.request(f"/nodes/{node_id}", missing=missing)
        if node is not None:
            require(
                not node["deleted"]
                and not node["retired"]
                and (kind is None or node["kind"] == kind),
                f"Unavailable {kind or 'record'} {node_id}",
            )
        return node

    def edges(self, node_id):
        context = self.request(f"/nodes/{node_id}/context")
        require(not context["truncated"], f"Dependency context truncated for {node_id}")
        return [(e["edge_type"], e["source"], e["target"]) for e in context["edges"]]

    def applied(self, anchor, owner):
        history = self.request(f"/nodes/{anchor}/history?limit=1")["items"]
        require(
            history and history[0]["operation"] == "created",
            f"No creation receipt for {anchor}",
        )
        change = self.request(f"/changesets/{history[0]['change_set_id']}")
        require(
            change["status"] == "applied"
            and change["actor_id"] == owner
            and change["result"],
            f"No owner-applied receipt for {anchor}",
        )
        return change["result"]
