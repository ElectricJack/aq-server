#!/usr/bin/env python3
"""Run with python3 scripts/test-epic-feature-retirement.py.

Requires Docker and locally loaded aq-graph:graph01-225329cd6acf-pinned
and postgres:17-bookworm images. The script creates disposable containers
and uses Graph's loopback HTTP API. It exercises Graph mutation receipts,
readiness, verification authority and parent completion. Repository revisions,
merge receipts and build results are fixture inputs. The checks in this
script model the proposed retirement policy.
"""

from contextlib import ExitStack
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener
from uuid import uuid4, uuid5


GRAPH_IMAGE = "aq-graph:graph01-225329cd6acf-pinned"
POSTGRES_IMAGE = "postgres:17-bookworm"
APPROVER = "scratch-approver"


def docker(*args):
    result = subprocess.run(
        ["docker", *args], capture_output=True, text=True, timeout=60
    )
    if result.returncode:
        raise RuntimeError(f"docker {args[0]} failed: {result.stderr.strip()}")
    return result.stdout.strip()


class Graph:
    def __init__(self, container, workspace, token):
        self.container = container
        self.workspace = workspace
        self.token = token
        self.http = build_opener(ProxyHandler({}))
        self.wait()

    def request(self, path, body=None, *, expected=200, token=None, scoped=True):
        prefix = f"/v1/workspaces/{self.workspace}" if scoped else ""
        request = Request(
            self.base + prefix + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": f"Bearer {token or self.token}",
                "Content-Type": "application/json",
            },
        )
        try:
            with self.http.open(request, timeout=10) as reply:
                code, payload = reply.status, json.load(reply)
        except HTTPError as reply:
            code, payload = reply.code, json.load(reply)
        assert code == expected, f"{path}: expected HTTP {expected}, got {code}: {payload}"
        return payload

    def wait(self):
        address = docker("port", self.container, "8741/tcp")
        assert address.startswith("127.0.0.1:") and "\n" not in address, address
        self.base = f"http://{address}"
        for _ in range(100):
            try:
                self.request("/health/ready", scoped=False)
                return
            except (OSError, AssertionError):
                time.sleep(0.1)
        raise RuntimeError(f"scratch Graph did not become ready: {docker('logs', self.container)}")

    def restart(self):
        docker("restart", self.container)
        self.wait()

    def mutate(self, request, **options):
        return self.request("/mutations", request, **options)

    def node(self, node_id):
        return self.request(f"/nodes/{node_id}")

    def ready_cleanup(self, *epics):
        ready = {node["id"] for node in self.request("/ready")["items"]}
        return ready & {epic.cleanup for epic in epics}


def start_graph(root, resources):
    workspace = uuid4()
    prefix = f"aq-epic-retirement-{workspace.hex[:12]}"
    network, pg, graph = (f"{prefix}-{part}" for part in ("net", "pg", "graph"))
    password, token = secrets.token_hex(24), secrets.token_hex(32)
    token_file = root / "graph-token"
    token_file.touch(mode=0o600)
    token_file.write_text(token)

    docker("network", "create", network)
    resources.callback(docker, "network", "rm", network)
    docker(
        "run", "-d", "--pull=never", "--name", pg, "--network", network,
        "--tmpfs", "/var/lib/postgresql/data",
        "-e", "POSTGRES_DB=scratch", "-e", f"POSTGRES_PASSWORD={password}",
        POSTGRES_IMAGE,
    )
    resources.callback(docker, "rm", "-fv", pg)
    for _ in range(100):
        result = subprocess.run(
            ["docker", "exec", pg, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
            capture_output=True, timeout=10,
        )
        if result.returncode == 0:
            break
        time.sleep(0.1)
    else:
        raise RuntimeError("scratch PostgreSQL did not become ready")

    docker(
        "run", "-d", "--pull=never", "--name", graph, "--network", network,
        "--user", f"{os.getuid()}:{os.getgid()}", "-p", "127.0.0.1::8741",
        "--mount", f"type=bind,src={token_file},dst=/run/graph-token,readonly",
        "-e", f"DATABASE_URL=postgres://postgres:{password}@{pg}/scratch",
        "-e", "AQ_GRAPH_BIND=0.0.0.0:8741",
        "-e", f"AQ_GRAPH_BOOTSTRAP_WORKSPACE={workspace}",
        "-e", f"AQ_GRAPH_BOOTSTRAP_ACTOR={APPROVER}",
        "-e", "AQ_GRAPH_BOOTSTRAP_TOKEN_FILE=/run/graph-token",
        GRAPH_IMAGE,
    )
    resources.callback(docker, "rm", "-fv", graph)
    return Graph(graph, workspace, token)


def batch(key, *operations):
    return {
        "idempotency_key": key,
        "reason": "Spec 0020 scratch acceptance",
        "operations": list(operations),
    }


def create(node_id, title, kind="task", **fields):
    if kind == "task":
        fields["task_state"] = "active"
    return {"op": "create_node", "id": node_id, "title": title, "kind": kind, **fields}


def update(node, **patch):
    return {
        "op": "update_node",
        "id": node["id"],
        "expected_version": node["version"],
        "patch": patch,
    }


def edge(source, target, edge_type):
    return {"op": "add_edge", "source": source, "target": target, "edge_type": edge_type}


class Epic:
    def __init__(self, workspace, name, flags):
        self.name = name
        self.parent = str(uuid5(workspace, f"epic/{name}"))
        self.registration = uuid5(workspace, f"{self.parent}/flag/1")
        self.gate = self.id("approval")
        self.cleanup = self.id("cleanup")
        key = str(self.registration)
        self.register = batch(
            f"register/{key}",
            create(key, f"Epic {name} flag", "knowledge", metadata={
                "scratch:flag": {
                    "epic": self.parent,
                    "generation": 1,
                    "project": str(workspace),
                    "repositories": flags,
                    "ordinary_default": False,
                    "permanent_behavior": f"Enable feature {name} in ordinary builds",
                },
            }),
        )
        self.ensure = batch(
            f"cleanup/{key}",
            create(self.parent, f"Epic {name}"),
            create(self.gate, f"Approve integrated feature {name}"),
            create(self.cleanup, f"Retire flag for epic {name}",
                   metadata={"scratch:registration": key}),
            edge(self.gate, self.cleanup, "blocks"),
            edge(self.parent, self.cleanup, "parent-child"),
            edge(self.cleanup, key, "motivated_by"),
        )

    def id(self, part):
        return str(uuid5(self.registration, part))

    def decision(self, graph, revision, choice, vetted):
        decision_id = self.id(f"decision/{revision}")
        record = create(
            decision_id, f"Epic {self.name} decision {revision}", "knowledge",
            content_type="decision",
            metadata={"scratch:decision": {
                "registration": str(self.registration),
                "revision": revision,
                "choice": choice,
                "vetted": vetted,
            }},
        )
        verify = {
            "op": "verify_node",
            "id": decision_id,
            "expected_version": 1,
            "verification": "verified",
            "evidence": [
                {"kind": "git", "source_id": repo, "repository": repo,
                 "commit": commit, "path": "src/main.rs"}
                for repo, commit in vetted.items()
            ],
        }
        operations = [record, verify]
        if choice == "approve":
            operations.append(update(
                graph.node(self.gate), task_state="completed",
                metadata={"scratch:decision": decision_id},
            ))
        return batch(f"decision/{decision_id}", *operations)

    def check_cleanup(self, graph):
        tree = graph.request(f"/nodes/{self.parent}/tree")
        assert [child["node"]["id"] for child in tree["children"]] == [self.cleanup]
        context = graph.request(f"/nodes/{self.cleanup}/context")
        blockers = {
            link["source"] for link in context["edges"]
            if link["edge_type"] == "blocks" and link["target"] == self.cleanup
        }
        assert blockers == {self.gate}

    def record_evidence(self, graph, stage, evidence):
        cleanup = graph.node(self.cleanup)
        metadata = {**cleanup["metadata"], "scratch:evidence": evidence}
        request = batch(f"{self.registration}/{stage}", update(cleanup, metadata=metadata))
        return request, graph.mutate(request)

    def close(self, graph):
        gate = graph.node(self.gate)
        if gate["task_state"] != "completed":
            return "approval pending"
        decision = graph.node(gate["metadata"]["scratch:decision"])
        assert decision["verification"] == "verified"
        assert decision["last_verified_by"] == APPROVER
        assert decision["metadata"]["scratch:decision"]["choice"] == "approve"
        assert decision["metadata"]["scratch:decision"]["registration"] == str(self.registration)

        registration = graph.node(str(self.registration))["metadata"]["scratch:flag"]
        cleanup = graph.node(self.cleanup)
        evidence = cleanup["metadata"].get("scratch:evidence", {})
        merges = evidence.get("merges", {})
        for repo in registration["repositories"]:
            if repo not in merges:
                return f"{repo}: merge pending"
        for repo in registration["repositories"]:
            if evidence.get("ordinary", {}).get(repo) != merges[repo]["sha"]:
                return f"{repo}: ordinary build pending"
        revisions = {repo: merges[repo]["sha"] for repo in registration["repositories"]}
        if evidence.get("integrated") != revisions:
            return "integrated check pending"

        request = batch(
            f"close/{self.registration}",
            update(cleanup, task_state="completed"),
            update(graph.node(self.parent), task_state="completed"),
        )
        return request, graph.mutate(request)


def scenario(graph):
    a = Epic(graph.workspace, "A", {"service": "scratch_a", "client": "scratch_client_a"})
    b = Epic(graph.workspace, "B", {"service": "scratch_b"})

    registration = graph.mutate(a.register)
    graph.request(f"/nodes/{a.cleanup}", expected=404)
    graph.restart()
    assert graph.mutate(a.register) == registration
    ensured = graph.mutate(a.ensure)
    assert graph.mutate(a.ensure) == ensured
    graph.mutate(b.register)
    graph.mutate(b.ensure)
    a.check_cleanup(graph)
    b.check_cleanup(graph)
    assert graph.ready_cleanup(a, b) == set()

    rejected = {"service": "1" * 40, "client": "2" * 40}
    graph.mutate(a.decision(graph, 1, "request_changes", rejected))
    assert a.close(graph) == "approval pending"
    assert graph.ready_cleanup(a, b) == set()

    vetted = {**rejected, "service": "3" * 40}
    approval = a.decision(graph, 2, "approve", vetted)
    writer = secrets.token_hex(32)
    graph.request("/credentials", {
        "actor_id": "scratch-writer", "token": writer, "capability": "write",
    })
    graph.mutate(approval, token=writer, expected=403)
    graph.request(f"/nodes/{a.id('decision/2')}", expected=404)
    assert graph.ready_cleanup(a, b) == set()

    approved = graph.mutate(approval)
    gate = graph.node(a.gate)
    assert graph.ready_cleanup(a, b) == {a.cleanup}
    graph.restart()
    assert graph.mutate(approval) == approved
    assert graph.node(a.gate) == gate
    assert graph.ready_cleanup(a, b) == {a.cleanup}
    assert graph.node(a.id("decision/1"))["metadata"]["scratch:decision"]["vetted"] == rejected
    assert graph.node(a.id("decision/2"))["metadata"]["scratch:decision"]["vetted"] == vetted

    service_sha, client_sha = "4" * 40, "5" * 40
    merges = {"service": {"pr": "scratch/service/1", "sha": service_sha}}
    first_request, first_receipt = a.record_evidence(graph, "first-merge", {"merges": merges})
    assert a.close(graph) == "client: merge pending"
    graph.mutate(batch("early-close", update(graph.node(a.parent), task_state="completed")),
                 expected=409)
    assert graph.node(a.parent)["task_state"] == "active"

    merges = {**merges, "client": {"pr": "scratch/client/1", "sha": client_sha}}
    a.record_evidence(graph, "second-merge", {"merges": merges})
    assert a.close(graph).endswith(": ordinary build pending")
    revisions = {"service": service_sha, "client": client_sha}
    a.record_evidence(graph, "ordinary", {"merges": merges, "ordinary": revisions})
    assert a.close(graph) == "integrated check pending"
    a.record_evidence(graph, "stale-integrated", {
        "merges": merges, "ordinary": revisions, "integrated": vetted,
    })
    assert a.close(graph) == "integrated check pending"
    a.record_evidence(graph, "integrated", {
        "merges": merges, "ordinary": revisions, "integrated": revisions,
    })
    closure, closed = a.close(graph)
    graph.restart()
    assert graph.mutate(closure) == closed

    # Recover the original partial receipt without altering the closed epic.
    assert graph.mutate(first_request) == first_receipt
    assert graph.node(a.parent)["task_state"] == "completed"
    assert graph.node(a.cleanup)["task_state"] == "completed"
    assert graph.node(b.parent)["task_state"] == "active"
    assert graph.node(b.cleanup)["task_state"] == "active"
    assert graph.ready_cleanup(a, b) == set()
    print("PASS: registration and approval replayed, A closed after both repos, and B stayed blocked.")


if __name__ == "__main__":
    if not __debug__:
        raise SystemExit("Run with Python assertions enabled.")
    with tempfile.TemporaryDirectory(prefix="aq-epic-retirement-") as directory, ExitStack() as resources:
        scenario(start_graph(Path(directory), resources))
