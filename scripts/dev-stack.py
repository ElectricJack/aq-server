#!/usr/bin/env python3
"""Private, replayable development configuration and public-API seed."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from dev_vault import ROOT, WORKSPACE, has_url_parameters, intelligence_section, paths, prepare, private_file

PROJECT = "e0000000-0000-4000-8000-000000000002"
REPOSITORY = "e0000000-0000-4000-8000-000000000003"
TASK = "e0000000-0000-4000-8000-000000000004"


def compose(env, *args, capture=True, input=None, timeout=None):
    result = subprocess.run(["docker", "compose", "-f", str(ROOT / "compose.yaml"),
                             "-f", str(ROOT / "compose.dev.yaml"), *args],
                            cwd=ROOT, env=dict(os.environ, **env), text=True,
                            capture_output=capture, input=input, timeout=timeout)
    if result.returncode:
        # Output is retained by callers; commands never contain credential values.
        raise RuntimeError(f"development Compose command exited {result.returncode}: {result.stderr}" if capture else "development Compose command failed")
    return result.stdout if capture else None


def enable_settlement():
    """Activate only after public-API seed has created the real owner/project."""
    env = prepare()
    state, _, _ = paths()
    with (state / ".settlement.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return enable_settlement_locked(env)


def enable_settlement_locked(env):
    state, _, vault = paths()
    server = os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:8080")
    owner = settlement_owner(state, server)
    pins = settlement_pins(env, state, owner)
    write_settlement(vault, pins)
    # Stop gracefully before acquiring the retained engine owner on a new process.
    compose(env, "stop", "server")
    compose(env, "run", "--rm", "--no-deps", "dev-state-init")
    compose(env, "up", "-d", "--no-build", "--wait", "--wait-timeout", "120", "server")
    status = http(server + "/api/playbooks/status")
    if status["namespace"]["namespace_id"] != pins["namespace_id"]:
        raise ValueError("settlement owner did not activate the retained namespace")
    return dict(namespace_id=pins["namespace_id"], grant_id=pins["grant_id"],
                expires_at_ms=pins["expires_at_ms"])


def settlement_owner(state, server):
    receipt = state / "seed.json"
    if not receipt.exists() or json.loads(receipt.read_text()).get("project_id") != PROJECT:
        raise ValueError("settlement requires a completed development seed")
    users = http(server + "/api/users")
    owners = [user for user in users if user["email"] == "owner@localhost" and user["role"] == "owner"]
    if len(owners) != 1:
        raise ValueError("settlement requires the current development owner")
    owner_id = http(server + "/api/v1/bootstrap")["data"]["principal"]["id"]
    require_seed_project(server)
    return owner_id


def require_seed_project(server):
    project = http(server + "/api/projects/" + PROJECT)
    if not project or not active_seed_binding(project["binding"]):
        raise ValueError("settlement project differs from the development seed")


def active_seed_binding(binding):
    return binding["graph_workspace_id"] == WORKSPACE and binding["enabled"]


def settlement_pins(env, state, owner_id):
    # Discover the actual socket UID; do not assume the image's postgres UID.
    socket_uid = int(compose(env, "exec", "-T", "postgres", "stat", "-c", "%u",
        "/run/aq-postgresql/.s.PGSQL.5432").strip())
    pins_path = state / "settlement.json"
    private_file(pins_path, json.dumps(dict(namespace_id=str(uuid.uuid4()), grant_id=str(uuid.uuid4()),
        owner_user_id=owner_id, socket_owner_uid=socket_uid,
        expires_at_ms=int(time.time() * 1000) + 23 * 60 * 60 * 1000), sort_keys=True).encode())
    pins = json.loads(pins_path.read_text())
    if pins["owner_user_id"] != owner_id or pins["socket_owner_uid"] != socket_uid:
        raise ValueError("retained settlement owner/socket differs; use a fresh development stack")
    return pins


def write_settlement(vault, pins):
    artifact = (ROOT / "scripts/dev-playbook-artifact.json").read_bytes()
    private_file(vault / "playbook-artifact.json", artifact)
    if (vault / "playbook-artifact.json").read_bytes() != artifact:
        raise ValueError("retained settlement artifact differs; use a fresh development stack")
    config = settlement_config(vault, pins)
    private_file(vault / "playbooks.toml", config.encode())
    if (vault / "playbooks.toml").read_text() != config:
        raise ValueError("retained settlement configuration differs; use a fresh development stack")


def settlement_config(vault, pins):
    socket_uid = pins["socket_owner_uid"]
    return (vault / "config.toml").read_text() + f'''
[playbooks]
enabled = true
profile = "human_review_v2"
engine_connection_file = "/run/aq-dev/playbook_dsn"
socket_owner_uid = {socket_uid}
engine_keyring_file = "/run/aq-dev/playbook_engine_keys"
drive_interval_ms = 1000
[playbooks.namespace]
namespace_id = "{pins['namespace_id']}"
generation = 1
name = "E0 local human settlement"
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
owner_user_id = "{pins['owner_user_id']}"
[playbooks.protection]
active_key_id = "dev-bridge-1"
[[playbooks.protection.keys]]
id = "dev-bridge-1"
file = "/run/aq-dev/playbook_bridge_key"
[[playbooks.deployments]]
artifact_file = "/run/aq-dev/playbook-artifact.json"
binding = "development-inspection-only"
grant_id = "{pins['grant_id']}"
expires_at_ms = {pins['expires_at_ms']}
[playbooks.deployments.scope]
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
capabilities = ["operation.get"]
[[playbooks.deployments.scope.recipients]]
kind = "task"
workspace_id = "{WORKSPACE}"
project_id = "{PROJECT}"
task_id = "{TASK}"
'''


def designation(local_fixture=False):
    keys = ("DEV_SCRATCH_REPO_URL", "DEV_SCRATCH_BASE_BRANCH", "DEV_SCRATCH_CREDENTIAL_REF")
    if any(not os.environ.get(key) for key in keys):
        raise ValueError("seed requires explicit DEV_SCRATCH_REPO_URL, DEV_SCRATCH_BASE_BRANCH and DEV_SCRATCH_CREDENTIAL_REF (use 'none' for a public repository); no scratch repository is designated by default")
    url, branch, credential = (os.environ[key] for key in keys)
    local_fixture = scratch_location(url, local_fixture)
    check_scratch_profile(branch, credential)
    return dict(url=url, base_branch=branch, credential_ref=credential, local_fixture=local_fixture)


def check_scratch_profile(branch, credential):
    if subprocess.run(["git", "check-ref-format", "refs/heads/" + branch], capture_output=True).returncode:
        raise ValueError("invalid DEV_SCRATCH_BASE_BRANCH")
    if credential != "none":
        raise ValueError("pinned aq-git does not enable named credential profiles; DEV_SCRATCH_CREDENTIAL_REF must explicitly be 'none' for a public scratch repository")


def scratch_location(url, local_fixture):
    parsed = urllib.parse.urlsplit(url)
    if dirty_repository_url(url, parsed):
        raise ValueError("DEV_SCRATCH_REPO_URL must not contain credentials, query, fragment or whitespace")
    if is_github_repository(parsed):
        return local_fixture
    if not parsed.scheme and allowed_local_path(url, local_fixture):
        return True
    raise ValueError("DEV_SCRATCH_REPO_URL must designate an HTTPS GitHub repository")


def dirty_repository_url(url, parsed):
    return has_url_parameters(parsed) or any(c.isspace() for c in url)


def is_github_repository(parsed):
    return (parsed.scheme, parsed.hostname, len(parsed.path.strip("/").split("/"))) == ("https", "github.com", 2)


def allowed_local_path(url, local_fixture):
    return local_scratch_remote(url) or (local_fixture and url.startswith("/fixture/"))


def local_scratch_remote(url):
    return (os.environ.get("DEV_SCRATCH_LOCAL_REMOTE") == "1"
            and url.startswith("/var/lib/aq/remotes/") and ".." not in url)


def http(url, body=None, token=None):
    headers = {"Content-Type": "application/json", "X-AQ-Request": "1"}
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        # Do not print request credentials or server response bodies.
        raise RuntimeError(f"public API returned HTTP {error.code}") from None


def git_request(state, operation, params=None, mutation=None):
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation,
                   params=params or {}, deadline_ms=10000)
    if operation != "capabilities":
        request.update(repository_id=REPOSITORY, expected_binding_generation=0 if operation == "repo.register" else 1)
    if mutation:
        request["operation_id"] = mutation
    if state.resolve() != paths()[0]:
        raise ValueError("Git request state must match the selected development stack")
    try:
        raw = compose(prepare(), "exec", "-T", "aq-git", "aq-git", "--socket", "/run/aq-git/central.sock",
                      "--input", "-", "--json", "request", input=json.dumps(request) + "\n", timeout=15)
    except subprocess.TimeoutExpired:
        raise RuntimeError("aq-git request timed out; replay the same seed operation") from None
    return git_response(raw, request)


def git_response(raw, request):
    if len(raw.encode()) > 65536 or not raw.endswith("\n"):
        raise RuntimeError("invalid bounded aq-git response")
    response = json.loads(raw)
    identity = (response.get("request_id"), response.get("protocol_version"))
    if identity != (request["request_id"], 1) or response.get("ok") is not True:
        raise RuntimeError("aq-git refused " + request["operation"])
    return response["result"]


def seed(local_fixture=False):
    choice = designation(local_fixture)  # Validate before creating files or contacting services.
    prepare()
    state, _, _ = paths()
    with (state / "seed.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return seed_locked(choice, choice["local_fixture"])


def verify_worker_credential(record, node_id, worker_url):
    if not matches_worker_record(record, node_id):
        raise ValueError("saved worker credential is incomplete or belongs to another node; existing identities are preserved")
    nonce = uuid.uuid4().hex
    try:
        reply = http(worker_url + "/v1/worker/probe", dict(nonce=nonce), record["credential"])
    except (RuntimeError, OSError):
        raise ValueError("saved worker credential could not be authenticated; existing identities are preserved") from None
    if not isinstance(reply, dict) or reply.get("nonce") != nonce:
        raise ValueError("worker credential probe response was not correlated; existing identities are preserved")


def matches_worker_record(record, node_id):
    if not isinstance(record, dict) or record.get("node_id") != node_id:
        return False
    token = record.get("credential")
    interval = record.get("heartbeat_interval_secs")
    return (isinstance(token, str) and token.startswith("aqn_")
            and isinstance(interval, int) and interval > 0)


def save_worker_credential(path, record):
    # Publish a complete private file atomically without replacing retained credentials.
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".dev-worker-", delete=False) as output:
            temporary = Path(output.name)
            json.dump(record, output)
            output.flush()
            os.fsync(output.fileno())
        os.link(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if temporary is not None:
            temporary.unlink()


def seed_locked(choice, local_fixture):
    state, secret, _ = paths()
    receipt = state / "seed.json"
    if receipt.exists() and json.loads(receipt.read_text())["designation"] != choice:
        raise ValueError("seed designation changed; existing project is preserved; use a new AQ_DEV_STATE_DIR and Compose project")
    # Pin replay inputs before the first mutation, so partial runs cannot switch repositories.
    receipt.write_text(json.dumps(dict(designation=choice), sort_keys=True) + "\n")
    graph = os.environ.get("AQ_DEV_GRAPH_URL", "http://127.0.0.1:8741")
    server = os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:8080")
    worker_url = os.environ.get("AQ_DEV_WORKER_URL", "http://127.0.0.1:8081")
    node_id = seed_worker(server, worker_url, secret)
    seed_graph_task(graph, secret)
    register_seed_repository(state, choice, local_fixture)
    seed_project(server, node_id)
    result = dict(designation=choice, project_id=PROJECT, task_id=TASK, workspace_id=WORKSPACE,
                  repository_id=REPOSITORY, node_id=node_id)
    receipt.write_text(json.dumps(result, sort_keys=True) + "\n")
    return result


def seed_worker(server, worker_url, secret):
    workers = [worker for worker in http(server + "/api/workers") if worker["name"] == "e0-local-development"]
    if len(workers) > 1 or any(worker["status"] == "revoked" for worker in workers):
        raise ValueError("seed worker identity is ambiguous or revoked; existing identities are preserved")
    credential_file = secret / "dev_worker.json"
    if workers:
        node_id = workers[0]["id"]
        worker = read_worker_credential(credential_file)
        verify_worker_credential(worker, node_id, worker_url)
        return node_id
    return enroll_worker(server, worker_url, credential_file)


def enroll_worker(server, worker_url, credential_file):
    if credential_file.exists() or credential_file.is_symlink():
        raise ValueError("saved worker credential has no matching worker; existing identities are preserved; use a new state, secrets and Compose project")
    enrollment = http(server + "/api/enrollments", {})
    worker = http(worker_url + "/v1/worker/enroll", dict(code=enrollment["code"],
        name="e0-local-development", worker_version="e0-seed", protocol_version=1))
    node_id = worker["node_id"]
    verify_worker_credential(worker, node_id, worker_url)
    save_worker_credential(credential_file, worker)
    return node_id


def read_worker_credential(credential_file):
    try:
        if credential_file.is_symlink() or not credential_file.is_file():
            raise ValueError()
        return json.loads(credential_file.read_text())
    except (ValueError, OSError):
        raise ValueError("saved worker credential is missing or invalid; existing identities are preserved; recover the credential or use a new state, secrets and Compose project") from None


def seed_graph_task(graph, secret):
    token = (secret / "graph_token").read_text().strip()
    workspace = graph + "/v1/workspaces/" + WORKSPACE
    if http(workspace + "/nodes/" + TASK, token=token) is None:
        http(workspace + "/mutations", dict(idempotency_key="e0-development-task-v1", reason="E0 local development seed",
             operations=[dict(op="create_node", id=TASK, kind="task", title="Local development scratch task",
                              body="Use only the owner-designated scratch repository.", task_state="active")]), token)


def register_seed_repository(state, choice, local_fixture):
    # Stable operation ID makes response-loss replay safe at the Git authority.
    git_request(state, "repo.register", dict(binding=dict(identity="e0-development", mode="central",
        remote_url=choice["url"], allow_file_transport=local_fixture,
        allowed_refs=["refs/heads/" + choice["base_branch"]],
        allowed_targets=["refs/heads/" + choice["base_branch"]])),
        mutation="e0000000-0000-4000-8000-000000000006")


def seed_project(server, node_id):
    # Roles and profiles need no seeding: the server installs its default documents at start.
    current = http(server + "/api/projects/" + PROJECT)
    if current is None:
        binding = dict(schema_version=1, project_id=PROJECT, name="Local scratch development", enabled=True,
            repository_id=REPOSITORY, repository_binding_generation=1, repository_adapter_reference="aq.git.configured",
            graph_adapter_reference="aq.graph.configured", graph_workspace_id=WORKSPACE,
            allowed_node_ids=[node_id], allowed_worker_groups=[],
            allowed_environments=["confined-worktree"], policy=dict(identity="local-development", revision="1", digest="b" * 64),
            context=[], ceilings=dict(permissions=[dict(operation="task.read", scope_reference="project:example")],
                context_bytes=8192, wall_time_ms=60000, memory_bytes=268435456, cpu_millis=60000, process_limit=8,
                cost_limit_microunits=None, currency=None, require_hard_cost_enforcement=False))
        http(server + "/api/projects/" + PROJECT,
            dict(operation_id="e0000000-0000-4000-8000-000000000008", expected_revision=0, binding=binding))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "seed", "settlement", "up", "down", "readiness"])
    parser.add_argument("--local-fixture", action="store_true", help="accept only a /fixture/ path owned by the disposable smoke test")
    args, remaining = parser.parse_known_args()
    try:
        result = run_command(args, remaining)
        if result is not None:
            print(json.dumps(result, sort_keys=True))
    except (ValueError, RuntimeError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)


def run_command(args, remaining):
    if args.command == "seed":
        result = seed(args.local_fixture)
        result["settlement"] = enable_settlement()
        return result
    if args.command == "settlement":
        return enable_settlement()
    environment = prepare()
    if args.command == "up":
        start_stack(environment, remaining)
    elif args.command == "down":
        compose(environment, "down", *remaining, capture=False)
    elif args.command == "readiness":
        compose(environment, "exec", "-T", "server", "aq-server", "dependency-readiness", "--vault", "/vault", capture=False)
    else:
        return environment
    return None


def start_stack(environment, remaining):
    candidate = json.loads((ROOT / "vendor/worker-candidate.json").read_text())
    environment.update(AQ_WORKER_SOURCE_REVISION=candidate["source_revision"], AQ_WORKER_ARCHIVE_SHA256=candidate["archive_sha256"], AQ_WORKER_VERSION=candidate["version"])
    builds = []
    for variable, service in (("AQ_SERVER_IMAGE", "server"), ("AQ_DEV_GRAPH_IMAGE", "aq-graph")):
        if not os.environ.get(variable):
            builds.append(service)
    if builds:
        compose(environment, "build", *builds, capture=False)
    compose(environment, "up", "--no-build", *remaining, capture=False)


if __name__ == "__main__":
    main()
