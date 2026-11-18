#!/usr/bin/env python3
"""MS1: profiled-execution configuration for a Claude Code agent in a
docker-local sandbox on the desktop node (aq-ms1 stack).

Ported from aq-worker scripts/e0-qual/definitions.py (the MS0/I0 Codex
qualification that ran end to end). Every value is a pinned observed artifact
(program SHA-256 on this host, public bodies printed by the pinned aq-worker)
or an operator decision recorded below. Nothing here admits or grants anything:
aq-server and aq-worker re-check all of it.

Roles, intelligence profiles and execution profiles are not built here (spec
0014): the server installs its default documents at start and they are edited
in Settings. No model is measured or listed; a worker runs the model an offer
names. Any other project is bound with scripts/bind-project.py.

Commands (paths come from the environment set by scripts/ms1-demo.sh):
  source       central aq-git: sync main, retain the base, record the handle
  configure    write intelligence.toml ([intelligence] + [profiled_execution])
               and the worker bundle
  apply        install the intelligence registry and save the scratch
               project's binding (default role and level, ceilings, nodes)
  observe      owner availability observation for the Claude subscription
  sync         (D1) central fetch of the project's remote main
  publication  (D1) delivery evidence and the remote's main
No credential bytes are read or written by this script.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.error
import urllib.request
import uuid

BASE = Path(os.environ.get("AQ_MS1_HOME", Path.home() / ".local/share/aq-ms1"))
STATE = BASE / "state"
NODE = BASE / "node"
RUNTIME = BASE / "runtime"
SERVER = os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:28080")
PROJECT = "e0000000-0000-4000-8000-000000000002"
REPOSITORY = "e0000000-0000-4000-8000-000000000003"
WORKSPACE = "e0000000-0000-4000-8000-000000000001"
ENVIRONMENT = "aq-ms1-docker-local-claude"
# The scratch project's defaults; both are shipped Settings documents.
DEFAULT_ROLE = os.environ.get("AQ_MS1_DEFAULT_ROLE", "developer")
DEFAULT_INTELLIGENCE = os.environ.get("AQ_MS1_DEFAULT_INTELLIGENCE", "standard-high")
CREDENTIAL = "owner-claude-subscription"
# Bump (AQ_MS1_CREDENTIAL_REVISION) when the credential state changes, e.g. the
# base file staged by import-login after the registry was first installed.
CREDENTIAL_REVISION = int(os.environ.get("AQ_MS1_CREDENTIAL_REVISION", "1"))
CREDENTIAL_VERSION = "2"
ACCOUNT = "owner-claude-subscription"
ENDPOINT = "claude-code"
HARNESS = "claude"
CLI_VERSION = "2.1.318"
PROVIDER = "anthropic"
MODEL = "claude-sonnet-5-5"
PROFILE = "claude-2.1.318-docker-local-result-local-v1"
CONTEXT_BYTES = 65536
MAX_OUTPUT_TOKENS = 32000
EXECUTION_MS = 900000
MEMORY_BYTES = 4 * 1024 * 1024 * 1024
CPU_MILLIS = 2000
PROCESS_LIMIT = 512
CLIENT_RESERVATION = 64 * 1024 * 1024
# aq-git native reservation: 2 * max_expanded + max_bundle + 1 MiB (aq-worker source.rs).
SOURCE_DISK_BYTES = 2 * 10737418240 + 1073741824 + 1024 * 1024
DISK_BYTES = 24 * 1024 * 1024 * 1024
# Bump on every content change: the server refuses changed bodies at a version.
# AQ_MS1_REVISION (default 1) bumps all three together after any pinned change
# (for example a rebuilt aq-worker) once an offer has retained the bodies.
_REV_FILE = BASE / "revision"
_REV = int(os.environ.get("AQ_MS1_REVISION") or (_REV_FILE.read_text().strip() if _REV_FILE.exists() else "1"))
PROFILED_REVISION = _REV
INTELLIGENCE_REVISION = _REV + 1
DEFINITION_VERSION = str(_REV)


def _accounting_source(provider, harness, parser_id, parser_digest, cache_read):
    """One [[intelligence.accounting.sources]] entry. `reference` and the layout
    policy are owner names (digest = SHA-256 of the id and version); the parser
    reference must equal the one the worker's observation engine stamps on its
    events (aq-worker crates/aq-observation-engine, cfa36431)."""
    def named(kind):
        name = f"aq.accounting.{kind}/{provider}-{harness}"
        return {"id": name, "version": "1", "digest": sha(name + "/v1")}
    parser = {"id": parser_id, "version": "0.0.1-engine", "digest": parser_digest}
    return {"provider": provider, "harness": harness, "reference": named("source"),
            "layout": {"policy": named("layout"), "input_includes_cache_read": cache_read,
                       "input_includes_cache_write": False, "output_includes_reasoning": True},
            "parsers": [{"parser": parser, "category_claim": parser, "priority": 1}]}


LONG_CONTRACT = {"id": "aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1", "version": "1",
                 "digest": "bd9abe1d9ef02970beb7946c007418de90a6066a146698ac0c075da1c1f2a28f"}
TERMINAL_CONTRACT = {"id": "aq.phase-terminal.registry-refresh-v1.contract", "version": "1",
                     "digest": "f9af55c49127a8cb93f03ef5bbb28313d13f94a221affa3e26d91d45bffd6c2d"}
NEGOTIATION_FEATURES = ["accepted_preparation_v2", "fresh_initial_confirmation_v2", "independent_execution_delivery_v2",
                        "registry_observation_continuity_v1"]
OWNER_KINDS = ["Project", "Registry", "Principal", "Inventory", "Availability", "Budget", "Context", "Git",
               "PrivateMaterialBacking"]
CAPABILITIES = [{"id": c, "version": 1} for c in (
    "aq.profiled-launch", "aq.terminal", "aq.harness.startup-context", "aq.harness.container-confinement",
    "aq.harness.stop")]
FEATURES = ["artifact_transfer_v1", "server_material_v1", "durable_host_journal_v1"]
# D3: the default completion policy; delivery to the GitHub remote (D1) is not
# required for MS1, retention of the attempt's commit in central aq-git is.
POLICY_BODY = {"schema_version": 1, "kind": "aq.default-completion.v1", "delivery": "not_required"}
# AQ_MS1_D1=1 (MS1 D1): D3 requires delivery; the project publishes by
# fast-forwarding main on its remote and its source follows main (the head AQ
# published), so a dependent task starts from its predecessor's commit.
D1 = os.environ.get("AQ_MS1_D1") == "1"
# Spec 0007 Q3 (swarm phase 1): enrolled service machines are allowed on the
# credential and the project binding without a node-id edit. The server's
# default is on; AQ_MS1_ENROLLED_NODES=0 writes an explicit false on both,
# which keeps only the listed node ids (allowed_node_ids stays as it is).
ENROLLED_NODES = os.environ.get("AQ_MS1_ENROLLED_NODES", "1") != "0"
TARGET = "refs/heads/main"
PUBLICATION = {"mode": "fast_forward", "target": TARGET}
if D1:
    POLICY_BODY = dict(POLICY_BODY, delivery="required")
STACK = os.environ.get("COMPOSE_PROJECT_NAME", "aq-ms1")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def require(condition, message):
    if not condition:
        raise SystemExit(f"ms1-profiled: {message}")


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        return "{" + ", ".join(f"{json.dumps(k)} = {toml_value(v)}" for k, v in value.items() if v is not None) + "}"
    raise ValueError(type(value).__name__)


def snake(kind):
    return "".join("_" + c.lower() if c.isupper() and i else c.lower() for i, c in enumerate(kind))


def private_write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = path.with_name(path.name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as out:
        out.write(text)
    os.replace(tmp, path)


class Builder:
    def __init__(self):
        self.definitions = []

    def raw(self, identifier, body, version="1"):
        require(not any(d["artifact"]["id"] == identifier for d in self.definitions), f"duplicate {identifier}")
        artifact = {"id": identifier, "version": version, "digest": sha(body)}
        self.definitions.append({"artifact": artifact, "kind": "definition", "body": body})
        return artifact

    def add(self, identifier, value, version="1"):
        return self.raw(identifier, canonical(value), version)


def http(url, body=None):
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json", "X-AQ-Request": "1"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            text = response.read()
            return json.loads(text) if text else None
    except urllib.error.HTTPError as error:
        if error.code == 404 and body is None:
            return None
        raise SystemExit(f"ms1-profiled: {url} -> {error.code} {error.read()[:600]!r}")


def git_request(operation, params=None, mutation=None, generation=1):
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation,
                   params=params or {}, deadline_ms=10000, repository_id=REPOSITORY,
                   expected_binding_generation=generation)
    if mutation:
        request["operation_id"] = mutation
    raw = subprocess.run(["docker", "exec", "-i", f"{STACK}-aq-git-1", "aq-git", "--socket",
                          "/run/aq-git/central.sock", "--input", "-", "--json", "request"],
                         input=json.dumps(request) + "\n", capture_output=True, text=True, timeout=120).stdout
    response = json.loads(raw)
    require(response.get("ok") is True, f"aq-git refused {operation}: {json.dumps(response.get('error'))[:500]}")
    return response["result"]


def source():
    """Retain the scratch repository's main head centrally (E0 qualification pattern)."""
    out = STATE / "ms1-source.json"
    if out.exists():
        return json.loads(out.read_text())
    branch = "refs/heads/main"
    shown = git_request("repo.show")
    synced = git_request("repo.sync", dict(refs=[branch]), mutation=str(uuid.uuid5(uuid.NAMESPACE_URL, "ms1-sync")))
    hex_oid = synced.get("refs", {}).get(branch)
    require(isinstance(hex_oid, str) and len(hex_oid) == 40, "sync gave no main object")
    base = dict(algorithm="sha1", hex=hex_oid)
    created = git_request("workspace.create", dict(base=base), mutation=str(uuid.uuid5(uuid.NAMESPACE_URL, "ms1-ws")))
    checkpoint = git_request("workspace.checkpoint", dict(workspace_id=created["workspace_id"], expected_generation=1,
                             expected_head=base, quiescent=True, mode="head"),
                             mutation=str(uuid.uuid5(uuid.NAMESPACE_URL, "ms1-checkpoint")))
    git_request("workspace.retire", dict(workspace_id=created["workspace_id"],
                expected_generation=checkpoint["workspace_generation"], expected_head=base, quiescent=True),
                mutation=str(uuid.uuid5(uuid.NAMESPACE_URL, "ms1-retire")))
    retained = git_request("retention.inspect", dict(handle_id=checkpoint["retention_handle"]))
    require(retained["state"] == "active" and any(r["oid"] == hex_oid for r in retained["refs"]),
            "retention handle does not hold the synced base")
    value = dict(base=base, retention_handle=checkpoint["retention_handle"], binding=shown)
    out.write_text(json.dumps(value, sort_keys=True) + "\n")
    return value


def pin(name, artifact_id):
    path = (RUNTIME / name).resolve()
    data = path.read_bytes()
    return {"path": str(path), "artifact": {"id": artifact_id, "version": "1", "digest": sha(data)},
            "bytes": len(data)}


def tmux_pin():
    path = Path("/usr/bin/tmux").resolve()
    data = path.read_bytes()
    return {"path": str(path), "artifact": {"id": "aq.ms1-host/tmux", "version": "1", "digest": sha(data)},
            "bytes": len(data)}


def build():
    seed = json.loads((STATE / "seed.json").read_text())
    src = json.loads((STATE / "ms1-source.json").read_text())
    worker = json.loads((BASE / "worker" / "node.json").read_text())
    node_id = (NODE / "config" / "worker.toml").read_text().split('node_id = "')[1].split('"')[0]
    # Spare enrollments (worker-*.toml) are allowed too, so a wedged node can
    # be swapped without a configuration revision.
    allowed = sorted({node_id} | {f.read_text().split('node_id = "')[1].split('"')[0]
                                  for f in (NODE / "config").glob("worker-*.toml")})
    worker_defs = json.loads(subprocess.run([str(RUNTIME / "aq-worker"), "ms1-definitions"], check=True,
                                            capture_output=True, text=True).stdout)
    template = worker["docker_local"]["templates"][0]
    scope = (f"aq-ms1 desktop node {node_id} only; docker-local sandbox template {template['digest']} (unadmitted, "
             f"I4a claude-rust-1); owner Claude subscription login {CREDENTIAL}@1; ElectricJack/aq-scratch main")
    aq_worker, host, client = (pin("aq-sandbox-driver", "aq.claude-docker-local-driver"),
                               pin("aq-session-host", "aq.ms1-host/aq-session-host"),
                               pin("aq-agent", "aq.ms1-host/aq-agent"))
    tmux = tmux_pin()
    permissions = [{"operation": "task.read", "scope_reference": f"project:{PROJECT}"},
                   {"operation": "knowledge.read", "scope_reference": f"project:{PROJECT}"},
                   {"operation": "source.write", "scope_reference": f"repository:{REPOSITORY}"}]
    scopes = [{"operation": p["operation"], "resource": p["scope_reference"]} for p in permissions]
    permission_digest = sha(canonical({"requested": scopes, "effective": scopes, "required": scopes,
                                       "omissions": []}))
    b = Builder()
    policy = b.add(f"aq.ms1-project-policy:{PROJECT}", POLICY_BODY)
    runtime = b.add("aq.ms1-runtime", {"schema_version": 1, "worker": aq_worker, "scope": scope}, DEFINITION_VERSION)
    environment = b.add(ENVIRONMENT, {
        "schema_version": 1, "scope": scope, "template": template,
        "images": {"driver": aq_worker, "host": host, "tmux": tmux, "client": client},
        "premises": ["node programs pinned by SHA-256 on the WSL2 host (not a read-only image)",
                     "Claude runs in a docker-local sandbox created by the C2 fixed launch policy on the dedicated "
                     "rootless Podman daemon (single-UID mode, CapEff 0, no-new-privs)",
                     "container confinement: the sandbox is the boundary (C6a; native unqualified on docker-local)"],
        "primary_image_reservation_bytes": 536870912,
        "additional_client_image_reservation_bytes": CLIENT_RESERVATION}, DEFINITION_VERSION)
    contract, startup = worker_defs["contract"], worker_defs["startup"]
    worker_src = Path(os.environ.get("AQ_MS1_WORKER_SRC", "/home/jkern/dev/aq-platform/wt/ms1-worker"))
    contract_body = (worker_src / "crates/aq-agent-result-local/artifacts/contract.json").read_text()
    startup_body = (worker_src / "crates/aq-agent-result-local/artifacts/startup.md").read_text()
    require(sha(contract_body) == contract["digest"] and sha(startup_body) == startup["digest"],
            "result-local bodies differ from the pinned aq-worker")
    b.raw("aq.agent-result-local", contract_body)
    b.raw("aq.agent-result-startup", startup_body)
    recipe = b.raw(worker_defs["recipe"]["id"], worker_defs["recipe"]["body"])
    require(recipe["digest"] == worker_defs["recipe"]["digest"], "recipe digest")
    client_manifest = {"schema_version": 1, "profile": "aq-agent-result-client-named-readonly-v1",
                       "contract": contract, "environment": environment,
                       "executable": {"path": client["path"], "artifact": client["artifact"]},
                       "executable_bytes": client["bytes"]}
    client_ref = b.add("aq.agent-result-client-installation", client_manifest, DEFINITION_VERSION)
    composite = {"schema_version": 1, "profile": PROFILE, "runtime": runtime, "environment": environment,
                 "client_manifest": client_ref, "client_executable": client["artifact"], "contract": contract,
                 "recipe": recipe, "permission_digest": permission_digest, "reserved_client_bytes": CLIENT_RESERVATION}
    adapter = b.add("aq.claude-result-local-adapter", composite, DEFINITION_VERSION)
    qualification = b.add("aq.ms1-runtime-qualification", {
        "schema_version": 1, "runtime": runtime, "adapter": adapter, "environment": environment, "scope": scope,
        "qualification": "pinned host programs, exact public result contract/startup and Claude docker-local recipe "
                         "bodies, owner-approved desktop Claude subscription profile",
        "not_evidence": ["no current node health or inventory", "no successful execution claimed by configuration"]},
        DEFINITION_VERSION)
    harness_q = b.add("aq.ms1-harness-qualification", {
        "schema_version": 1, "runtime": runtime, "qualification": qualification, "adapter": adapter,
        "environment": environment, "driver": aq_worker, "host": host, "tmux": tmux, "scope": scope,
        "max_output_tokens": MAX_OUTPUT_TOKENS,
        "max_output_tokens_basis": "AQ planning reservation only; not a Claude Code setting"}, DEFINITION_VERSION)
    # Stable body: a credential identity/version can never change its definition.
    credential = b.add(CREDENTIAL, {
        "schema_version": 1, "provider": PROVIDER, "account_reference": ACCOUNT, "endpoint_reference": ENDPOINT,
        "scope": "aq-ms1 desktop docker-local nodes only (allowed_node_ids in [intelligence])",
        "attestation": "owner decision 2026-11-04: Claude Code on the owner's Claude subscription (setup-token), "
                       "no API key",
        "secret": "long-lived setup-token delivered by the server material owner, env-only in the sandbox"},
        CREDENTIAL_VERSION)
    # The startup renderer is one constant rule (aq-server context_budget.rs
    # `Renderer::rule`); no model is measured. The worker bundle still names it
    # until the worker drops its renderer pin (spec 0014 §8).
    rendering = canonical({"rendering": "utf8_double_newline_v1", "schema_version": 1})
    renderer = {"id": f"aq.startup-renderer/{sha(rendering)}", "version": "1", "digest": sha(rendering)}
    source_profile = b.add("aq.ms1-default-git-profile", {
        "schema_version": 1, "checkout_profile": "no_attributes_v1", "max_expanded_bytes": 10737418240,
        "max_bundle_bytes": 1073741824, "source_disk_bytes": SOURCE_DISK_BYTES,
        "scope": "unchanged default aq-git bounds; the actual service capabilities must match before admission"})
    owners = []
    for kind in OWNER_KINDS:
        authority = b.add(f"aq.ms1-owner/{kind}", {
            "schema_version": 1, "kind": snake(kind), "server_source": "aq-ms1",
            "scope": "actual authenticated aq-ms1 server owner; read and fenced again at execution"})
        owners.append({"kind": snake(kind), "authority": authority})
    phase_policy = b.add("aq.accepted-preparation-refresh-policy/ms1", {
        "schema_version": 1, "contract": LONG_CONTRACT, "runtime": runtime, "qualification": qualification,
        "phase": {"schema_version": 3, "maximum_preparation_ms": 30000, "maximum_execution_lease_ms": EXECUTION_MS,
                  "maximum_observation_ms": 2000},
        "source_profile": source_profile, "source_disk_bytes": SOURCE_DISK_BYTES, "owners": owners})
    b.add("aq.phase-refresh-execution-qualification/ms1", {
        "schema_version": 1, "contract": LONG_CONTRACT, "runtime": runtime, "runtime_qualification": qualification,
        "proposal_policy": phase_policy,
        "adapters": "registry_observation_continuity_current_project_principal_inventory_health_unissued_graph_retention_private_v1"})
    proof = b.add("aq.ms1-physical-proof-producer", {
        "schema_version": 1, "worker": aq_worker, "helper": host, "tmux": tmux,
        "scope": "retained session-host guardian/incarnation plus kernel ECHILD; the sandbox driver is the pane "
                 "program and stops its sandbox before it exits"})
    b.add("aq.phase-terminal-refresh-qualification/ms1", {
        "schema_version": 1, "contract": TERMINAL_CONTRACT, "runtime": runtime, "runtime_qualification": qualification,
        "proof_producer": proof})
    inventory_adapter = b.add("aq.ms1-scoped-presence-adapter", {
        "schema_version": 1, "worker": aq_worker,
        "scope": "grant-bounded scoped material presence; public inventory lists provisionable profiles only"})
    inventory_evidence = b.add("aq.ms1-scoped-presence-evidence", {
        "schema_version": 1, "adapter": inventory_adapter, "runtime": runtime, "qualification": qualification,
        "scope": scope, "not_evidence": "no live presence or inventory receipt is created by configuration"})
    presence = {"schema_version": 1, "protocol": "aq.scoped-material-presence.v1", "runtime": runtime,
                "runtime_qualification": qualification, "adapter": inventory_adapter,
                "adapter_qualification": inventory_evidence,
                "inventory_semantics": "provisionable_only_and_scoped_presence_v1",
                "observation_protocol": "grant_issuance_bounded_boottime_v1", "maximum_age_ms": 30000,
                "maximum_ttl_ms": 30000}
    presence_ref = b.add("aq.scoped-material-presence-qualification/ms1", presence)
    material = b.add("aq.server-material", {
        "schema_version": 1, "server_source": "aq-ms1", "adapter": "aq.server-material", "version": 1,
        "scope": "authenticated exact-attempt private material owner; owner Claude login for the desktop node only"})
    definitions = b.definitions
    sizes = [len(d["body"].encode()) for d in definitions]
    require(len(definitions) <= 64 and max(sizes) <= 2 * 1024 * 1024, "definition bounds")
    harness = {"harness": HARNESS, "cli_version": CLI_VERSION, "adapter": adapter, "environment": environment,
               "qualification": harness_q,
               "capabilities": CAPABILITIES,
               "limits": {"context_bytes": CONTEXT_BYTES, "max_output_tokens": MAX_OUTPUT_TOKENS,
                          "wall_time_ms": EXECUTION_MS, "memory_bytes": MEMORY_BYTES, "cpu_millis": CPU_MILLIS,
                          "process_limit": PROCESS_LIMIT}}
    profiled = {"revision": PROFILED_REVISION,
                "qualified_runtimes": [{"runtime": runtime, "qualification": qualification,
                                        "capabilities": CAPABILITIES, "features": FEATURES,
                                        "material_delivery": {"adapter": material, "version": 1}}],
                "projects": [{"project_id": PROJECT, "enabled": True, "priority": 0,
                              "source": {"kind": "named_ref", "reference": TARGET} if D1 else
                                        {"kind": "immutable",
                                         "object": {"format": src["base"]["algorithm"], "hex": src["base"]["hex"]}},
                              "retention_handle": src["retention_handle"]}],
                "definition_bodies": definitions}
    intelligence = {"revision": INTELLIGENCE_REVISION, "material_store": "/var/lib/aq/login-material",
                    "credentials": [{
                        "kind": "subscription-login",
                        "credential": {"profile": credential, "provider": PROVIDER, "account_reference": ACCOUNT,
                                       "endpoint_reference": ENDPOINT, "revision": CREDENTIAL_REVISION,
                                       "enabled": True, "provisionable": False},
                        "allowed_node_ids": allowed,
                        "allow_enrolled_nodes": ENROLLED_NODES,
                        "secret_file": "/var/lib/aq/credentials/owner-claude-subscription",
                        "delivery_mode": "server_material_v1"}],
                    "qualified_harnesses": [harness],
                    # Without a source the server refuses the sessions' observations and
                    # the stall ladder has no progress evidence.
                    "accounting": {"sources": [
                        _accounting_source("anthropic", "claude", "claude-v1-9a9b8e8",
                                           "3b4c8920ee0fa58d587a3431d0dcef31f49d3490f063775febd5642054b92251", False),
                        _accounting_source("openai", "codex", "codex-rollout-67f0994",
                                           "048a19804f680b53ed5a14bc33f65984b4842da457c3bf42e1c443ccd9e5cad9", True)]}}
    state_dir = NODE / "state"
    docker = dict(worker["docker_local"])
    host_config = {
        "root": str(state_dir / "host"), "helper": {"path": host["path"], "artifact": host["artifact"]},
        "tmux": {"path": tmux["path"], "artifact": tmux["artifact"]},
        "driver": {"path": aq_worker["path"], "artifact": aq_worker["artifact"]}, "renderer": renderer,
        "columns": 120, "rows": 40, "docker_local": docker, "template": template["digest"],
        "claude_executable": "/opt/aq/bin/claude", "max_workspace_bytes": 256 * 1024 * 1024}
    placeholder = {"schema_version": 1, "note": "unused by the Claude docker-local executor"}
    native = {
        "schema_version": 1, "state_dir": str(state_dir), "git_socket": str(state_dir / "git.sock"),
        "git_root": str(state_dir / "git"),
        "capacity": {"slots": 1, "memory_bytes": MEMORY_BYTES, "cpu_millis": CPU_MILLIS, "disk_bytes": DISK_BYTES},
        "runtime": {"runtime": runtime, "qualification": qualification}, "adapter": adapter, "renderer": renderer,
        "adapter_manifest": str(NODE / "config/adapter.json"), "adapter_manifest_digest": sha(canonical(placeholder)),
        "capabilities": CAPABILITIES, "features": FEATURES,
        "negotiation": {"contract": LONG_CONTRACT, "features": NEGOTIATION_FEATURES},
        "source_profile": source_profile, "maximum_preparation_ms": 30000,
        "inventory_definition": str(NODE / "config/inventory.json"), "inventory_reference": presence_ref,
        "inventory_adapter": inventory_adapter, "inventory_evidence": inventory_evidence,
        "harness_qualifications": [harness_q], "inventory_ttl_ms": 30000,
        "executor": {"kind": PROFILE, "composite_definition": str(NODE / "config/composite.json"),
                     "settings": {"host": host_config, "environment": environment,
                                  "permissions_digest": permission_digest}}}
    files = {"native.json": canonical(native), "adapter.json": canonical(placeholder),
             "composite.json": canonical(composite), "inventory.json": canonical(presence)}
    return dict(definitions=definitions, profiled=profiled, intelligence=intelligence, files=files, policy=policy,
                permissions=permissions, renderer=renderer, node_id=node_id, seed=seed, allowed=allowed)


def binding_rev(built, current):
    seed = built["seed"]
    return dict(schema_version=1, project_id=PROJECT, name=current.get("name", "E0 local development scratch"),
                enabled=True, repository_id=REPOSITORY, repository_binding_generation=1,
                repository_adapter_reference="aq.git.configured", graph_adapter_reference="aq.graph.configured",
                graph_workspace_id=seed["workspace_id"],
                allowed_node_ids=built["allowed"], allowed_worker_groups=[],
                allowed_environments=[ENVIRONMENT],
                policy=dict(identity=built["policy"]["id"], revision="1", digest=built["policy"]["digest"]),
                context=[], ceilings=dict(permissions=built["permissions"], context_bytes=CONTEXT_BYTES,
                                          wall_time_ms=EXECUTION_MS, memory_bytes=MEMORY_BYTES, cpu_millis=CPU_MILLIS,
                                          process_limit=PROCESS_LIMIT, cost_limit_microunits=None, currency=None,
                                          require_hard_cost_enforcement=False),
                # The server omits a default from the binding it returns (a true flag, the
                # default role and level), so only a non-default is sent: an explicit default
                # would differ from the current binding on every apply.
                **{key: value for key, value, default in (
                    ("default_role", DEFAULT_ROLE, "developer"),
                    ("default_intelligence", DEFAULT_INTELLIGENCE, "standard-high")) if value != default},
                **({"role_notes": current["role_notes"]} if current.get("role_notes") else {}),
                **({} if ENROLLED_NODES else {"allow_enrolled_nodes": False}),
                **({"publication": PUBLICATION} if D1 else {}))


def configure():
    built = build()
    # Real table headers: this text is appended after other vault tables.
    toml = "# MS1 (generated by scripts/ms1-profiled.py; no secrets). The login enters via import-login.\n"
    for table in ("intelligence", "profiled"):
        toml += "[intelligence]\n" if table == "intelligence" else "[profiled_execution]\n"
        for key, value in built[table].items():
            if value is not None:
                toml += f"{key} = {toml_value(value)}\n"
    # Private result-media store (dashboard Results view: results.media.read
    # and results.media.upload). The root lives on the server's /var/lib/aq
    # volume (created 0700 by `ms1-demo.sh media`); the storage id is stable.
    media_id = BASE / "state" / "result-media-storage-id"
    if not media_id.exists():
        private_write(media_id, str(uuid.uuid4()) + "\n")
    toml += "[result_media]\n"
    toml += 'root = "/var/lib/aq/result-media"\n'
    toml += f'storage_id = "{media_id.read_text().strip()}"\n'
    toml += "uploads_enabled = true\n"
    import tomllib
    parsed = tomllib.loads(toml)
    require(set(parsed) == {"intelligence", "profiled_execution", "result_media"}, "TOML shape")
    target = Path(os.environ.get("AQ_DEV_INTELLIGENCE_FILE", BASE / "intelligence.toml"))
    private_write(target, toml)
    for name, text in built["files"].items():
        private_write(NODE / "config" / name, text)
    public = dict(intelligence_sha256=sha(toml),
                  definitions=[{"id": d["artifact"]["id"], "digest": d["artifact"]["digest"]}
                               for d in built["definitions"]],
                  renderer=built["renderer"], policy=built["policy"], node_id=built["node_id"],
                  worker_files={k: sha(v) for k, v in built["files"].items()})
    (BASE / "configure.json").write_text(json.dumps(public, indent=1, sort_keys=True) + "\n")
    return public


def apply():
    # The intelligence registry is installed from the vault by an owner request.
    http(SERVER + "/api/intelligence/register", {})
    built = build()
    current = http(SERVER + "/api/projects/" + PROJECT)
    require(current is not None, "seed the project first")
    for kind, name in (("roles", DEFAULT_ROLE), ("intelligence-profiles", DEFAULT_INTELLIGENCE)):
        require(http(f"{SERVER}/api/settings/{kind}/{name}") is not None, f"no {kind} document {name!r} exists")
    binding = binding_rev(built, current.get("binding", {}))
    if current.get("binding") != binding:
        http(SERVER + "/api/projects/" + PROJECT, dict(
            operation_id=str(uuid.uuid5(uuid.NAMESPACE_URL, "ms1-binding-" + sha(canonical(binding)))),
            expected_revision=current.get("revision"), binding=binding))
    after = http(SERVER + "/api/projects/" + PROJECT)
    snapshot = http(SERVER + "/api/settings/profiles/snapshot")
    return dict(profiles={k: snapshot.get(k) for k in ("active_digest", "revision")},
                binding_revision=after.get("revision"))


def observe():
    """Owner availability observation for the Claude subscription candidate
    (selection refuses stale or unknown availability; max age is 30 s)."""
    import time
    registry = http(SERVER + "/api/intelligence/registry")
    credential = registry["credentials"][0]
    now = int(time.time() * 1000)
    return http(SERVER + "/api/intelligence/observations", dict(
        operation_id=str(uuid.uuid4()), expected_revision=registry["revision"], sequence=now,
        scope=dict(credential_revision=credential["revision"], provider=PROVIDER,
                   credential_profile=credential["profile"], account_reference=ACCOUNT,
                   endpoint_reference=ENDPOINT, model=MODEL, node_id=None),
        state="available", ttl_ms=3600000, cooldown_ms=0))


def sync():
    """D1: central fetch of the project's remote main (a fresh observation)."""
    synced = git_request("repo.sync", dict(refs=[TARGET]), mutation=str(uuid.uuid4()))
    return {k: synced.get(k) for k in ("observation_id", "refs", "completed_at_ms")}


def publication():
    """D1: delivery evidence and source heads (server DB) and the remote's main."""
    query = ("SELECT json_agg(json_build_object('attempt_id',attempt_id,'source_operation_id',source_operation_id,"
             "'state',state,'recorded_at_ms',recorded_at_ms,'evidence',evidence) ORDER BY recorded_at_ms) "
             "FROM profiled_delivery_evidence")
    raw = subprocess.run(["docker", "exec", "-i", f"{STACK}-postgres-1", "sh", "-ec",
                          'PGPASSWORD="$(cat /run/secrets/postgres_password)" psql -h 127.0.0.1 -U agent_q -d agent_q -tAc "$0"',
                          query], capture_output=True, text=True, timeout=30).stdout.strip()
    remote = subprocess.run(["docker", "exec", f"{STACK}-aq-git-1", "git", "--git-dir",
                             "/var/lib/aq/remotes/aq-scratch.git", "log", "--format=%H %P %s", "-5", "main"],
                            capture_output=True, text=True, timeout=30).stdout.splitlines()
    return dict(delivery_evidence=json.loads(raw) if raw else [], remote_main=remote)


if __name__ == "__main__":
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    commands = dict(source=source, configure=configure, apply=apply, observe=observe, sync=sync,
                    publication=publication)
    require(command in commands, f"usage: ms1-profiled.py {'|'.join(commands)}")
    print(json.dumps(commands[command](), indent=1, sort_keys=True))
