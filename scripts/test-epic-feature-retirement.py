#!/usr/bin/env python3
"""Two-epic acceptance of the spec 0020 owner CLI on disposable AQ Graph.

Requires rustc, Docker and locally loaded aq-graph:graph01-225329cd6acf-pinned
and postgres:17-bookworm images. Graph, authentication, persistent changesets,
task readiness and compilation are real. Recorded evidence uses fixture identities.
"""

from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import time
from typing import NamedTuple
from urllib.error import URLError
from urllib.request import urlopen
from uuid import uuid4

from epic_feature_retirement import (
    META,
    Graph,
    Retirement,
    RetirementError,
    create,
    patch,
)

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


class ScratchGraph:
    def __init__(self, container, workspace, token, token_file):
        self.container = container
        self.token_file = token_file
        self.graph = Graph("http://127.0.0.1", str(workspace), token)
        self.wait()

    @property
    def origin(self):
        return self.graph.base.split("/v1/workspaces/", 1)[0]

    def wait(self):
        address = docker("port", self.container, "8741/tcp")
        assert address.startswith("127.0.0.1:") and "\n" not in address
        self.graph.base = f"http://{address}/v1/workspaces/{self.graph.workspace}"
        for _ in range(100):
            try:
                with urlopen(self.origin + "/health/ready", timeout=1):
                    return
            except (URLError, TimeoutError, ConnectionResetError):
                time.sleep(0.1)
        raise RuntimeError(
            f"scratch Graph did not become ready: {docker('logs', self.container)}"
        )

    def restart(self):
        docker("restart", self.container)
        self.wait()

    def cli(self, *args):
        result = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).with_name("epic_feature_retirement.py")),
                "--graph-origin",
                self.origin,
                "--workspace",
                self.graph.workspace,
                "--owner",
                APPROVER,
                "--token-file",
                str(self.token_file),
                *map(str, args),
            ],
            text=True,
            capture_output=True,
            timeout=30,
            check=True,
        )
        return json.loads(result.stdout)


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
        "run",
        "-d",
        "--pull=never",
        "--name",
        pg,
        "--network",
        network,
        "--tmpfs",
        "/var/lib/postgresql/data",
        "-e",
        "POSTGRES_DB=scratch",
        "-e",
        f"POSTGRES_PASSWORD={password}",
        POSTGRES_IMAGE,
    )
    resources.callback(docker, "rm", "-fv", pg)
    for _ in range(100):
        result = subprocess.run(
            ["docker", "exec", pg, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
            capture_output=True,
            timeout=10,
        )
        if result.returncode == 0:
            break
        time.sleep(0.1)
    else:
        raise RuntimeError("scratch PostgreSQL did not become ready")

    docker(
        "run",
        "-d",
        "--pull=never",
        "--name",
        graph,
        "--network",
        network,
        "--user",
        f"{os.getuid()}:{os.getgid()}",
        "-p",
        "127.0.0.1::8741",
        "--mount",
        f"type=bind,src={token_file},dst=/run/graph-token,readonly",
        "-e",
        f"DATABASE_URL=postgres://postgres:{password}@{pg}/scratch",
        "-e",
        "AQ_GRAPH_BIND=0.0.0.0:8741",
        "-e",
        f"AQ_GRAPH_BOOTSTRAP_WORKSPACE={workspace}",
        "-e",
        f"AQ_GRAPH_BOOTSTRAP_ACTOR={APPROVER}",
        "-e",
        "AQ_GRAPH_BOOTSTRAP_TOKEN_FILE=/run/graph-token",
        GRAPH_IMAGE,
    )
    resources.callback(docker, "rm", "-fv", graph)
    return ScratchGraph(graph, workspace, token, token_file)


def mutate(graph, *operations):
    return graph.request(
        "/mutations", {"idempotency_key": str(uuid4()), "operations": list(operations)}
    )


def rejected(message, operation):
    try:
        operation()
    except RetirementError as error:
        assert message in str(error), error
        return
    raise AssertionError(f"Operation was expected to refuse: {message}")


def proof(description):
    return {
        "artifact_id": str(uuid4()),
        "sha256": hashlib.sha256(description.encode()).hexdigest(),
    }


def ready(graph, *tasks):
    ids = {node["id"] for node in graph.request("/ready")["items"]}
    return {task for task in tasks if task in ids}


SERVICE = "Acme/service"
CLIENT = "Acme/client"


def registration(graph, name, repositories, **extra):
    epic = str(uuid4())
    mutate(graph, create(epic, "task", f"Epic {name}", task_state="active"))
    return {
        "epic_id": epic,
        "project_id": graph.workspace,
        "generation": 1,
        "repositories": repositories,
        "ordinary_default": False,
        "permanent_behavior": f"Enable feature {name} in ordinary builds",
        **extra,
    }


def decision(number, choice, vetted):
    return {
        "revision": number,
        "choice": choice,
        "vetted": vetted,
        "evidence": proof(f"vetted revision {number}"),
    }


def scratch_build(directory, source, expected, *flags):
    fixture = directory / "feature_fixture.rs"
    binary = directory / "feature_fixture"
    fixture.write_text(source)
    features = [arg for flag in flags for arg in ("--cfg", f'feature="{flag}"')]
    build = subprocess.run(
        ["rustc", str(fixture), "-o", str(binary), *features],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if expected is None:
        assert build.returncode != 0 and "removed_feature" in build.stderr, build.stderr
        return
    assert build.returncode == 0, build.stderr
    result = subprocess.run(
        [str(binary)],
        capture_output=True,
        text=True,
        timeout=5,
        check=True,
    )
    assert result.stdout.strip() == expected


def merge(repo, number, head):
    return {
        "kind": "merge",
        "approval_revision": 2,
        "repository": repo,
        "pull_request": f"https://example.test/{repo}/pull/{number}",
        "sha": head,
        "evidence": proof(f"merge receipt {repo}/{number}"),
    }


def ordinary(repo, head, passed):
    return {
        "kind": "ordinary",
        "approval_revision": 2,
        "repository": repo,
        "sha": head,
        "passed": passed,
        "flag_absent": passed,
        "evidence": proof(f"ordinary test {repo}"),
    }


def verify_feature_combinations(directory):
    initial = """fn main() {
        println!("{}{}", cfg!(feature = "feature_a") as u8,
                        cfg!(feature = "feature_b") as u8);
    }"""
    for expected, flags in (
        ("00", ()),
        ("01", ("feature_b",)),
        ("10", ("feature_a",)),
        ("11", ("feature_a", "feature_b")),
    ):
        scratch_build(directory, initial, expected, *flags)


def verify_adoption(graph, owner):
    # Match the staged Epic 0 shape on disposable Graph. The existing IDs
    # and existing prerequisite survive adoption and stay blocked.
    old_gate, old_cleanup = str(uuid4()), str(uuid4())
    mutate(
        graph,
        create(old_gate, "task", "Existing approval"),
        create(old_cleanup, "task", "Existing cleanup"),
        {
            "op": "add_edge",
            "edge_type": "blocks",
            "source": old_gate,
            "target": old_cleanup,
        },
    )
    adopted = registration(
        graph,
        "adoption",
        {SERVICE: "legacy_flag"},
        adopt={"approval": old_gate, "cleanup": old_cleanup},
    )
    adopted_id = owner.register(adopted)["registration"]
    state = owner.status(adopted_id)
    assert (state["approval"], state["cleanup"]) == (old_gate, old_cleanup)
    assert graph.node(old_cleanup)["metadata"][META]["registration"] == adopted_id
    assert ready(graph, old_cleanup) == set()


def verify_client_repair(owner, registration_id, directory, heads):
    broken_client_sha, client_sha = heads
    broken_merge = merge(CLIENT, 3, broken_client_sha)
    broken_merge_receipt = owner.evidence(registration_id, broken_merge)
    broken_client = """#[cfg(feature = "FEATURE_CLIENT_A")]
    fn removed_feature() -> u8 { 1 }
    fn main() { println!("{}", removed_feature()); }"""
    scratch_build(directory, broken_client, None)
    broken_build = ordinary(CLIENT, broken_client_sha, False)
    broken_build_receipt = owner.evidence(registration_id, broken_build)
    rejected("ordinary build or flag removal check failed", lambda: owner.close(registration_id))
    client_repair = merge(CLIENT, 4, client_sha)
    rejected("must supersede", lambda: owner.evidence(registration_id, client_repair))
    client_repair["supersedes_merge"] = owner.status(registration_id)["merge_receipts"][CLIENT]
    rejected(
        "must have a different SHA",
        lambda: owner.evidence(registration_id, {**client_repair, "sha": broken_client_sha}),
    )
    repaired_merge_receipt = owner.evidence(registration_id, client_repair)
    rejected(
        "must supersede",
        lambda: owner.evidence(registration_id, {**client_repair, "sha": "7" * 40}),
    )
    assert owner.evidence(registration_id, broken_merge) == broken_merge_receipt
    assert owner.evidence(registration_id, broken_build) == broken_build_receipt
    assert owner.evidence(registration_id, client_repair) == repaired_merge_receipt
    rejected("ordinary build evidence pending", lambda: owner.close(registration_id))


class RegisteredEpics(NamedTuple):
    primary: str
    secondary: str
    primary_status: dict
    secondary_cleanup: str
    registration_receipt: dict


def verify_registration(scratch, directory, owner, specs):
    graph = scratch.graph
    spec_a, spec_b = specs
    a, first = owner.register_record(spec_a)
    assert owner.status(a)["status"] == "Registration pending"
    assert graph.node(owner.status(a)["cleanup"], missing=True) is None
    scratch.restart()
    file_a = directory / "a.json"
    file_a.write_text(json.dumps(spec_a))
    registered_a = scratch.cli("register", file_a)
    assert registered_a["registered"] == first
    assert scratch.cli("register", file_a) == registered_a
    b = owner.register(spec_b)["registration"]
    addon = {"repositories": {CLIENT: "FEATURE_CLIENT_A"}}
    assert owner.amend(a, addon) == owner.amend(a, addon)
    status_a, status_b = scratch.cli("status", a), owner.status(b)
    cleanup_a, cleanup_b = status_a["cleanup"], status_b["cleanup"]
    assert cleanup_a != cleanup_b
    assert ready(graph, cleanup_a, cleanup_b) == set()
    for status in (status_a, status_b):
        tree = graph.request(f"/nodes/{status['epic_id']}/tree")
        assert {child["node"]["id"] for child in tree["children"]} == {
            status["cleanup"],
            status["closure_gate"],
        }
    return RegisteredEpics(a, b, status_a, cleanup_b, registered_a)


def verify_owner_approval(scratch, owner, epics):
    graph = scratch.graph
    a, status_a = epics.primary, epics.primary_status
    cleanup_a, cleanup_b = status_a["cleanup"], epics.secondary_cleanup
    rejected_revisions = {SERVICE: "1" * 40, CLIENT: "2" * 40}
    request_changes = decision(1, "request_changes", rejected_revisions)
    assert owner.decide(a, request_changes) == owner.decide(a, request_changes)
    assert owner.status(a)["status"] == "Changes requested"
    assert ready(graph, cleanup_a, cleanup_b) == set()

    vetted = {**rejected_revisions, SERVICE: "3" * 40}
    approval = decision(2, "approve", vetted)
    writer_token, admin_token = secrets.token_hex(32), secrets.token_hex(32)
    for actor, token, capability in [
        ("scratch-writer", writer_token, "write"),
        ("other-admin", admin_token, "admin"),
    ]:
        graph.request(
            "/credentials",
            {"actor_id": actor, "token": token, "capability": capability},
        )
    writer = Graph(scratch.origin, graph.workspace, writer_token)
    admin = Graph(scratch.origin, graph.workspace, admin_token)
    gate = graph.node(status_a["approval"])
    rejected("HTTP 403", lambda: mutate(writer, patch(gate, task_state="completed")))
    rejected(
        "HTTP 403",
        lambda: mutate(writer, patch(graph.node(cleanup_a), task_state="active")),
    )
    rejected("different actor", lambda: Retirement(admin, APPROVER).decide(a, approval))
    rejected(
        "different actor", lambda: Retirement(writer, APPROVER).decide(a, approval)
    )
    assert ready(graph, cleanup_a, cleanup_b) == set()

    approved = owner.decide(a, approval)
    assert owner.status(a)["status"] == "Approved · flag removal pending"
    assert ready(graph, cleanup_a, cleanup_b) == {cleanup_a}
    scratch.restart()
    writer = Graph(scratch.origin, graph.workspace, writer_token)
    assert owner.decide(a, approval) == approved
    rejected(
        "different committed content",
        lambda: owner.decide(a, {**approval, "vetted": rejected_revisions}),
    )
    rejected(
        "cannot change",
        lambda: owner.amend(a, {"repositories": {"Acme/extra": "feature_a"}}),
    )
    assert ready(graph, cleanup_a, cleanup_b) == {cleanup_a}
    return writer, vetted


def verify_integrated_build(owner, registration_id, builds, vetted):
    integrated = {
        "kind": "integrated",
        "approval_revision": 2,
        "passed": False,
        "revisions": {repo: build["sha"] for repo, build in builds.items()},
        "builds": {repo: build["evidence"] for repo, build in builds.items()},
        "evidence": proof("integrated check before repair"),
    }
    rejected(
        "cleanup merge SHAs",
        lambda: owner.evidence(registration_id, {**integrated, "revisions": vetted}),
    )
    owner.evidence(registration_id, integrated)
    rejected("Integrated acceptance failed", lambda: owner.close(registration_id))
    owner.evidence(
        registration_id,
        {
            **integrated,
            "passed": True,
            "evidence": proof("integrated check after repair"),
        },
    )


def verify_service_build(owner, registration_id, directory, service_sha):
    service_after = (
        """fn main() { println!("1{}", cfg!(feature = "feature_b") as u8); }"""
    )
    assert "feature_a" not in service_after
    scratch_build(directory, service_after, "10")
    scratch_build(directory, service_after, "11", "feature_b")
    service_merge = merge(SERVICE, 1, service_sha)
    recorded = owner.evidence(registration_id, service_merge)
    service_build = ordinary(SERVICE, service_sha, True)
    owner.evidence(registration_id, service_build)
    rejected(f"{CLIENT}: cleanup merge pending", lambda: owner.close(registration_id))
    return service_merge, recorded, service_build


def verify_sibling_gate(graph, writer, epic, close):
    sibling = str(uuid4())
    mutate(
        graph,
        create(sibling, "task", "Other required epic work", task_state="active"),
        {
            "op": "add_edge",
            "edge_type": "parent-child",
            "source": epic,
            "target": sibling,
        },
    )
    rejected(f"Epic task {sibling} is still open", lambda: close())
    mutate(writer, patch(graph.node(sibling), task_state="completed"))


def scenario(scratch, directory):
    graph = scratch.graph
    owner = Retirement(graph, APPROVER)
    spec_a = registration(graph, "A", {SERVICE: "feature_a"})
    spec_b = registration(graph, "B", {SERVICE: "feature_b"})
    verify_feature_combinations(directory)
    epics = verify_registration(scratch, directory, owner, (spec_a, spec_b))
    writer, vetted = verify_owner_approval(scratch, owner, epics)
    a, b, status_a = epics.primary, epics.secondary, epics.primary_status
    cleanup_a, cleanup_b = status_a["cleanup"], epics.secondary_cleanup
    service_sha, broken_client_sha, client_sha = "4" * 40, "5" * 40, "6" * 40
    service_merge, recorded, service_build = verify_service_build(owner, a, directory, service_sha)

    # Normal task settlement can complete the cleanup, but the protected
    # closure gate keeps the epic open until the independent evidence arrives.
    mutate(writer, patch(graph.node(cleanup_a), task_state="completed"))
    rejected(
        "HTTP 409",
        lambda: mutate(
            writer, patch(graph.node(spec_a["epic_id"]), task_state="completed")
        ),
    )
    rejected(
        "HTTP 403",
        lambda: mutate(
            writer, patch(graph.node(status_a["closure_gate"]), task_state="canceled")
        ),
    )
    rejected(
        "must match", lambda: owner.evidence(a, ordinary(CLIENT, client_sha, False))
    )

    verify_client_repair(owner, a, directory, (broken_client_sha, client_sha))

    client_build = ordinary(CLIENT, client_sha, True)
    client_after = 'fn main() { println!("1"); }'
    assert "FEATURE_CLIENT_A" not in client_after
    scratch_build(directory, client_after, "1")
    owner.evidence(a, client_build)
    verify_integrated_build(owner, a, {SERVICE: service_build, CLIENT: client_build}, vetted)

    verify_sibling_gate(graph, writer, spec_a["epic_id"], lambda: owner.close(a))
    closed = owner.close(a)
    scratch.restart()
    assert owner.close(a) == closed
    assert owner.evidence(a, service_merge) == recorded
    assert owner.register(spec_a) == epics.registration_receipt
    assert owner.status(a)["status"] == "Retired"
    assert graph.node(spec_a["epic_id"])["task_state"] == "completed"
    assert owner.status(b)["status"] == "Approval pending"
    assert graph.node(cleanup_b)["task_state"] == "draft"
    assert ready(graph, cleanup_a, cleanup_b) == set()

    verify_adoption(graph, owner)

    print(
        "PASS: two independent epics, atomic owner release, closure evidence, restart replay and blocked adoption"
    )


if __name__ == "__main__":
    if not __debug__:
        raise SystemExit("Run with Python assertions enabled.")
    with (
        tempfile.TemporaryDirectory(prefix="aq-epic-retirement-") as tmp,
        ExitStack() as resources,
    ):
        directory = Path(tmp)
        scenario(start_graph(directory, resources), directory)
