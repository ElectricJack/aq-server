#!/usr/bin/env python3
"""E1c: sandbox load through the rootless sandbox daemon cannot starve the stack.

Requires the dedicated rootless sandbox daemon (Podman >= 5 compat API, systemd
cgroup driver, cgroup v2) at AQ_SANDBOX_SOCKET (default
$XDG_RUNTIME_DIR/aq-sandbox/podman.sock), a systemd user manager, and the published
Dockerfile.worker digest (deploy/worker-image.env, or AQ_WORKER_IMAGE). The host's
main Docker daemon runs only the production stack, never sandboxes.
"""
import http.client
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import socket
import subprocess
import time
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("compose_prod_smoke", ROOT / "scripts/compose-prod-smoke.py")
prod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prod)

SANDBOX_IMAGE = "docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
MAIN_SOCKETS = ("/var/run/docker.sock", "/run/docker.sock")
SLICE_TEMPLATE = ROOT / "deploy/systemd/aq-sandbox-user.slice.conf"
# Budget conservatively against decimal 8 GB; deployment must verify MemTotal.
VPS = {"cpus": 4.0, "memory": 8_000_000_000}
HOST_MEMORY_MIN = 1_000_000_000
STACK = ("postgres", "server", "aq-graph", "aq-git")
LOAD_SECONDS = 30
READINESS_DEADLINE = 5.0  # server healthcheck timeout
SPINNER_SANDBOXES, SPINNERS_EACH, HOGS = 4, 4, 2
API = "/v1.41"
PLACEHOLDERS = {
    "AQ_SERVER_IMAGE": "example/aq-server@sha256:" + "a" * 64,
    "AQ_SUPERVISOR_HOST_IMAGE": "example/aq-supervisor-host@sha256:" + "b" * 64,
    "AQ_WORKER_IMAGE": "example/aq-worker@sha256:" + "c" * 64,
    "AQ_PUBLIC_HOSTNAME": "aq.example.com",
    "AQ_SANDBOX_UID": "2000", "AQ_SANDBOX_GID": "2000",
    "AQ_SANDBOX_SOCKET_DIR": "/run/user/2000/aq-sandbox",
    "AQ_SUPERVISOR_CONFIG_DIR": "/etc/aq/supervisor",
    "AQ_WORKER_CONFIG_DIR": "/etc/aq/worker",
    "AQ_VAULT_DIR": "/etc/aq/vault", "AQ_UPDATER_SPOOL": "/var/lib/aq-updater",
    "AQ_SECRETS_DIR": "/etc/aq/secrets",
}


class Sandbox(http.client.HTTPConnection):
    """Docker-compatible API over the sandbox daemon's Unix socket."""
    def __init__(self, path):
        super().__init__("sandbox", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(self.path)


def api(path, method, url, body=None, ok=(200, 201, 204, 304)):
    connection = Sandbox(path)
    connection.request(method, API + url, body=None if body is None else json.dumps(body),
                       headers={"Content-Type": "application/json"})
    response = connection.getresponse()
    data = response.read()
    connection.close()
    if response.status not in ok:
        raise RuntimeError(f"sandbox API {method} {url}: {response.status} {data[:300]!r}")
    return json.loads(data) if data.strip().startswith((b"{", b"[")) else data


def sandbox_socket():
    """Refuse anything but a dedicated rootless daemon with cgroup delegation."""
    runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    path = os.environ.get("AQ_SANDBOX_SOCKET", runtime + "/aq-sandbox/podman.sock")
    real = os.path.realpath(path)
    assert real not in MAIN_SOCKETS and path not in MAIN_SOCKETS, "refusing the main Docker socket: " + path
    assert Path(real).is_socket(), "sandbox daemon socket missing: " + path
    info = api(real, "GET", "/info")
    assert "name=rootless" in info.get("SecurityOptions", []), "sandbox daemon is not rootless"
    # A daemon without cgroup delegation (e.g. dind-rootless: CgroupDriver none)
    # silently discards per-sandbox limits.
    assert info.get("CgroupDriver") == "systemd" and str(info.get("CgroupVersion")) == "2", \
        ("sandbox daemon lacks systemd cgroup v2 delegation", info.get("CgroupDriver"), info.get("CgroupVersion"))
    return real, {"socket": path, "server_version": info.get("ServerVersion"),
                  "cgroup_driver": info["CgroupDriver"], "security_options": info["SecurityOptions"]}


def slice_properties():
    values = dict(re.findall(r"^(\w+)=(\S+)$", SLICE_TEMPLATE.read_text(), re.M))
    for key in ("CPUQuota", "MemoryMax", "MemorySwapMax", "TasksMax"):
        assert key in values, "slice template missing " + key
    return values


def size(value):
    units = {"K": 2**10, "M": 2**20, "G": 2**30}
    return int(value[:-1]) * units[value[-1]] if value[-1] in units else int(value)


def merged_config(*profiles):
    env = dict(os.environ, **PLACEHOLDERS)
    for name in ("COMPOSE_PROFILES", "COMPOSE_FILE"):
        env.pop(name, None)
    return json.loads(subprocess.check_output(
        ["docker", "compose", "-f", str(ROOT / "compose.yaml"), "-f", str(ROOT / "compose.workers.yaml"),
         *[arg for profile in profiles for arg in ("--profile", profile)],
         "config", "--format", "json"], cwd=ROOT, env=env))


def binds(service):
    return [v for v in service.get("volumes", []) if v["type"] == "bind"]


def validate_production(config, slice_values, host_budget=VPS):
    """Static production contract: limits everywhere, sockets only where intended."""
    services = config["services"]
    assert {"aq-supervisor-host", "aq-worker-cloud", "cloudflared", *STACK} <= set(services)
    long_running = {"cpus": 0.0, "memory": 0}
    limits = {}
    for name, service in services.items():
        for key in ("cpus", "mem_limit", "memswap_limit", "pids_limit"):
            assert service.get(key), f"{name} missing {key}"
        memory = int(service["mem_limit"])
        assert int(service["memswap_limit"]) == memory, name + " may swap beyond its memory limit"
        limits[name] = {"cpus": float(service["cpus"]), "memory": memory, "pids": int(service["pids_limit"])}
        assert prod.DIGEST.fullmatch(service["image"]), "unpinned image: " + name
        assert service["read_only"] and service["cap_drop"] == ["ALL"], name
        assert "no-new-privileges:true" in service["security_opt"], name
        assert not service.get("privileged") and service.get("pid") != "host", name
        assert not service.get("ports"), "host ports on " + name
        for volume in service.get("volumes", []):
            for path in (volume.get("source", ""), volume["target"]):
                assert "docker.sock" not in path and not path.rstrip("/").endswith(("/run/docker", "/var/run")), \
                    f"{name} mounts a Docker socket path: {path}"
        if service.get("restart") != "no" and name not in ("graph-db-init", "server-db-init"):
            long_running["cpus"] += limits[name]["cpus"]
            long_running["memory"] += memory
    sandbox_cpus = int(slice_values["CPUQuota"].rstrip("%")) / 100
    sandbox_memory = size(slice_values["MemoryMax"])
    assert slice_values["MemorySwapMax"] == "0"
    total_cpus = long_running["cpus"] + sandbox_cpus
    total_memory = long_running["memory"] + sandbox_memory
    assert total_cpus <= host_budget["cpus"], ("CPU budget exceeds VPS/host", total_cpus)
    assert total_memory <= host_budget["memory"] - HOST_MEMORY_MIN, ("memory budget leaves under 1 GB for the host", total_memory)

    worker = services["aq-worker-cloud"]
    assert set(worker["networks"]) == {"workers"}, "worker must not join edge/database"
    assert worker["environment"]["AQ_SERVER_URL"] == "https://" + PLACEHOLDERS["AQ_PUBLIC_HOSTNAME"]
    assert worker["environment"]["DOCKER_HOST"] == "unix:///run/aq-sandbox/podman.sock"
    assert worker["user"] == PLACEHOLDERS["AQ_SANDBOX_UID"] + ":" + PLACEHOLDERS["AQ_SANDBOX_GID"]
    assert {(b["source"], b["target"]) for b in binds(worker)} == {
        (PLACEHOLDERS["AQ_WORKER_CONFIG_DIR"], "/config"), (PLACEHOLDERS["AQ_SANDBOX_SOCKET_DIR"], "/run/aq-sandbox")}
    worker_targets = {v["target"] for v in worker["volumes"] if v["type"] == "volume"}
    assert worker_targets == {"/state", "/var/lib/aq-worker/leases"}, "journal and lease cache volumes"
    assert not worker.get("secrets")
    assert worker["depends_on"]["aq-worker-state-init"]["condition"] == "service_completed_successfully"
    initializer = services["aq-worker-state-init"]
    assert initializer["image"] == worker["image"] and initializer["user"] == "0:0"
    assert initializer["network_mode"] == "none" and not binds(initializer)
    assert {v["target"] for v in initializer["volumes"]} == worker_targets

    if "aq-worker-boat" in services:
        validate_boat(services)

    supervisor = services["aq-supervisor-host"]
    assert set(supervisor["networks"]) <= {"database", "supervisor"}, "supervisor is not a worker node"
    assert [(b["source"], b["target"], b.get("read_only")) for b in binds(supervisor)] == [
        (PLACEHOLDERS["AQ_SUPERVISOR_CONFIG_DIR"], "/config", True)]
    assert {v["target"] for v in supervisor["volumes"]} == {"/config", "/host", "/workspace", "/native-homes"}
    assert set(supervisor.get("cap_add", [])) <= {"CHOWN", "DAC_OVERRIDE", "FOWNER", "KILL", "SETGID", "SETUID"}
    grace = supervisor["stop_grace_period"]
    assert re.fullmatch(r"\d+s", grace) and int(grace[:-1]) > 10, grace
    # Supervisor credentials never reach workers or any other service.
    supervisor_sources = {v.get("source") for v in supervisor["volumes"]} - {None}
    for name, service in services.items():
        if name != "aq-supervisor-host":
            assert not supervisor_sources & {v.get("source") for v in service.get("volumes", [])}, \
                f"{name} shares supervisor state or credentials"
    return {"service_limits": limits, "sandbox_slice": slice_values,
            "budget": {"cpus": total_cpus, "memory_bytes": total_memory, "host": host_budget}}


def validate_boat(services):
    boat, worker = services["aq-worker-boat"], services["aq-worker-cloud"]
    assert boat["user"] == "10001:10001" and set(boat["networks"]) == {"workers"}
    assert boat["environment"]["AQ_SERVER_URL"] == worker["environment"]["AQ_SERVER_URL"]
    assert "DOCKER_HOST" not in boat["environment"]
    assert boat["entrypoint"] == ["/bin/sh", "/boat-entrypoint.sh"]
    assert {v["target"] for v in boat["volumes"]} == {
        "/config", "/boat-entrypoint.sh", "/state", "/var/lib/aq-worker/leases"}
    assert all(v.get("read_only") for v in binds(boat))
    assert [s["source"] for s in boat["secrets"]] == ["boat_attempt_key"]
    boat_sources = {v["source"] for v in boat["volumes"]}
    assert not boat_sources & {v["source"] for v in worker["volumes"]}, "Boat shares cloud identity/state"
    boat_init = services["aq-worker-boat-state-init"]
    assert boat["depends_on"]["aq-worker-boat-state-init"]["condition"] == "service_completed_successfully"
    assert boat_init["network_mode"] == "none" and not binds(boat_init)
    assert boat_init["image"] == boat["image"] and boat_init["user"] == "0:0"
    assert {v["source"] for v in boat_init["volumes"]} == {
        v["source"] for v in boat["volumes"] if v["type"] == "volume"}
    for name, service in services.items():
        if name != "aq-worker-boat":
            assert "boat_attempt_key" not in {s["source"] for s in service.get("secrets", [])}


def validate_running(stack, limits, names=STACK):
    """Live engine HostConfig carries the declared limits."""
    observed = {}
    for name in names:
        cid = stack.compose("ps", "-q", name).stdout.strip()
        host = json.loads(stack.run("docker", "inspect", cid).stdout)[0]["HostConfig"]
        want = limits[name]
        assert host["NanoCpus"] == int(want["cpus"] * 1e9), (name, host["NanoCpus"])
        assert host["Memory"] == want["memory"] and host["MemorySwap"] == want["memory"], name
        assert host["PidsLimit"] == want["pids"], name
        observed[name] = {k: host[k] for k in ("NanoCpus", "Memory", "MemorySwap", "PidsLimit")}
    return observed


def systemctl(*args, check=True):
    return subprocess.run(["systemctl", "--user", *args], capture_output=True, text=True, check=check)


def slice_usage(path):
    stat = Path("/sys/fs/cgroup" + path, "cpu.stat").read_text()
    return int(re.search(r"^usage_usec (\d+)$", stat, re.M).group(1))


def worker_setup(sock, evidence):
    """Run the actual production worker against a disposable server identity.

    Only this smoke's server additionally joins the workers network, allowing a
    local HTTP foundation heartbeat without a real Access hostname/token. The
    production worker's user, mounts, command and hardening stay in the overlay.
    B2 admission is fenced by this worker revision; no tasks are submitted.
    """
    default_image = next(line.removeprefix("AQ_WORKER_IMAGE=") for line in
                         (ROOT / "deploy/worker-image.env").read_text().splitlines()
                         if line.startswith("AQ_WORKER_IMAGE="))
    image = os.environ.get("AQ_WORKER_IMAGE", default_image)
    assert prod.DIGEST.fullmatch(image), "set AQ_WORKER_IMAGE to the published Dockerfile.worker digest"
    candidate = json.loads((ROOT / "vendor/worker-candidate.json").read_text())
    node_id, credential = str(uuid.uuid4()), "aqn_" + secrets.token_hex(32)
    uid, gid = Path(sock).stat().st_uid, Path(sock).stat().st_gid

    def setup(stack):
        directory = stack.directory / "worker-config"
        (directory / "agent-q").mkdir(parents=True)
        # Only the enrolled foundation config is secret; never print it.
        (directory / "agent-q/worker.toml").write_text(
            f'server_url = "http://server:8081"\nnode_id = "{node_id}"\ncredential = "{credential}"\n')
        (directory / "agent-q/worker.toml").chmod(0o600)
        (directory / "controller.json").write_text(json.dumps({
            "state_dir": "/state/controller",
            "docker_local": {"socket": "/run/aq-sandbox/podman.sock", "templates": []},
            "node": {"runtime": {"id": "aq-worker", "version": candidate["version"], "digest": "0" * 64},
                     "capabilities": [], "features": [], "templates": [], "max_concurrent_sandboxes": 2,
                     "spend_ceiling": None, "capacity": {"memory_bytes": 8 * 2**30, "cpu_millis": 4000,
                     "disk_bytes": 50 * 2**30, "max_preparing": 1}}}))
        stack.env.update(AQ_WORKER_IMAGE=image, AQ_WORKER_CONFIG_DIR=str(directory),
                         AQ_SANDBOX_SOCKET_DIR=str(Path(sock).parent),
                         AQ_SANDBOX_UID=str(uid), AQ_SANDBOX_GID=str(gid),
                         AQ_PUBLIC_HOSTNAME="aq.example.com",
                         AQ_SUPERVISOR_HOST_IMAGE=PLACEHOLDERS["AQ_SUPERVISOR_HOST_IMAGE"],
                         AQ_SUPERVISOR_CONFIG_DIR=str(stack.directory / "unused-supervisor"))
        fixture = json.loads((stack.directory / "smoke.json").read_text())
        fixture["services"].update({
            "aq-supervisor-host": {"profiles": ["supervisor"]},
            "aq-worker-cloud": {"profiles": ["worker-smoke"]},
            "aq-worker-state-init": {"profiles": ["worker-smoke"]},
            "server": {"networks": {"workers": {}}}})
        (stack.directory / "smoke.json").write_text(json.dumps(fixture))

    def start(stack):
        def sql(query):
            return stack.compose("exec", "-T", "postgres", "sh", "-ec",
                'export PGPASSWORD="$(cat /run/secrets/postgres_password)"; exec psql -v ON_ERROR_STOP=1 -U agent_q -d aq_server -tAc "$1"',
                "sh", query).stdout.strip()
        hashed = hashlib.sha256(credential.encode()).hexdigest()
        sql(f"INSERT INTO nodes (id,name,credential_hash,worker_version,protocol_version,enrolled_at) "
            f"VALUES ('{node_id}','compose-worker-smoke','{hashed}','{candidate['version']}',1,now())")
        stack.compose("up", "-d", "--no-build", "aq-worker-cloud")
        cid = stack.compose("ps", "-q", "aq-worker-cloud").stdout.strip()
        stack.wait(lambda: sql(f"SELECT last_seen_at IS NOT NULL FROM nodes WHERE id='{node_id}'") == "t",
                   "actual worker foundation heartbeat", 30)
        def controller_ready():
            logs = stack.compose("logs", "--no-color", "aq-worker-cloud").stdout
            return "node controller starting" in logs and "admission fenced; foundation heartbeat remains active" in logs
        stack.wait(controller_ready, "docker-local controller recovery and admission fence", 15)
        version = stack.compose("exec", "-T", "aq-worker-cloud", "aq-worker", "--version").stdout.strip()
        assert version == "aq-worker " + candidate["version"], version
        observed = json.loads(stack.compose("exec", "-T", "aq-worker-cloud", "cat",
                                           "/usr/share/agent-q/worker-candidate.json").stdout)
        assert observed == candidate, "running worker differs from pinned source"
        ownership = stack.compose("exec", "-T", "aq-worker-cloud", "stat", "-c", "%u:%g:%a",
                                  "/state", "/var/lib/aq-worker/leases").stdout.splitlines()
        assert ownership == [f"{uid}:{gid}:700"] * 2, ownership
        journal = json.loads(stack.compose("exec", "-T", "aq-worker-cloud", "cat",
                                            "/state/controller/journal.json").stdout)
        assert journal["state"]["identity"]["node_id"] == node_id
        state = json.loads(stack.run("docker", "inspect", cid).stdout)[0]
        assert state["State"]["Running"] and state["RestartCount"] == 0, state["State"]
        assert state["Config"]["User"] == f"{uid}:{gid}"
        assert state["HostConfig"]["ReadonlyRootfs"] and state["HostConfig"]["CapDrop"] == ["ALL"]
        assert set(state["NetworkSettings"]["Networks"]) == {stack.project + "_workers"}
        evidence["worker"] = {"image": image, "version": version, "source_revision": candidate["source_revision"],
                              "node_id": node_id, "heartbeat": "recorded by actual server",
                              "controller": "docker-local connected; journal persisted; admission fenced",
                              "volume_ownership": ownership, "restart_count": state["RestartCount"]}

    return setup, start


def sandbox_load_does_not_starve_server_health():
    evidence = {"test": "sandbox_load_does_not_starve_server_health"}
    sock, evidence["sandbox_daemon"] = sandbox_socket()
    setup, start_worker = worker_setup(sock, evidence)
    slice_values = slice_properties()
    evidence["production_config"] = validate_production(merged_config(), slice_values)
    # Stand-in for the production user-<uid>.slice drop-in on this shared host user.
    unit = "aqsandbox" + uuid.uuid4().hex[:10] + ".slice"
    units = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"), "systemd/user")
    unit_file = units / unit
    sandboxes = []
    try:
        with prod.production_stack("aq-e1c-", evidence, compose_files=("compose.workers.yaml",), setup=setup) as stack:
            start_worker(stack)
            evidence["running_limits"] = validate_running(
                stack, evidence["production_config"]["service_limits"], (*STACK, "aq-worker-cloud"))
            units.mkdir(parents=True, exist_ok=True)
            unit_file.write_text("[Slice]\n" + "".join(f"{k}={v}\n" for k, v in slice_values.items()))
            systemctl("daemon-reload")
            systemctl("start", unit)
            cgroup = systemctl("show", "-p", "ControlGroup", "--value", unit).stdout.strip()
            assert cgroup, "transient sandbox slice has no cgroup"
            encoded = urllib.parse.quote(SANDBOX_IMAGE, safe="")
            try:
                api(sock, "GET", f"/images/{encoded}/json")
            except RuntimeError:
                api(sock, "POST", "/images/create?fromImage=" + encoded)
            def launch(kind, command, cpus, memory):
                # C2 fixed launch policy shape plus the aggregate slice parent.
                created = api(sock, "POST", f"/containers/create?name={unit[:-6]}-{kind}-{len(sandboxes)}", {
                    "Image": SANDBOX_IMAGE, "Cmd": ["sh", "-c", command],
                    "Labels": {"aq.smoke": unit},
                    "HostConfig": {"Privileged": False, "CapDrop": ["ALL"], "SecurityOpt": ["no-new-privileges"],
                                   "NanoCpus": int(cpus * 1e9), "Memory": memory, "MemorySwap": memory,
                                   "PidsLimit": 64, "NetworkMode": "none", "ReadonlyRootfs": True,
                                   "CgroupParent": unit}})
                sandboxes.append(created["Id"])
                api(sock, "POST", f"/containers/{created['Id']}/start")
                return created["Id"]
            spin = "for i in $(seq %d); do while :; do :; done & done; wait" % SPINNERS_EACH
            for _ in range(SPINNER_SANDBOXES):
                launch("cpu", spin, SPINNERS_EACH / 2, 256 * 2**20)
            hogs = [launch("memory", "tail /dev/zero", 1, 512 * 2**20) for _ in range(HOGS)]
            quota = int(slice_values["CPUQuota"].rstrip("%")) / 100
            demand = SPINNER_SANDBOXES * SPINNERS_EACH / 2  # sum of per-sandbox NanoCpus
            assert demand > quota, "load must exceed the aggregate quota"
            ids = {name: stack.compose("ps", "-q", name).stdout.strip() for name in STACK}
            started_usage, started_at = slice_usage(cgroup), time.monotonic()
            samples, latencies, failures = 0, [], []
            while time.monotonic() - started_at < LOAD_SECONDS:
                began = time.monotonic()
                code, report = stack.readiness()
                latency = time.monotonic() - began
                latencies.append(latency)
                if code != 0 or latency > READINESS_DEADLINE:
                    failures.append({"latency": round(latency, 2), "code": code, "report": report})
                for name, cid in ids.items():
                    health = json.loads(stack.run("docker", "inspect", cid).stdout)[0]["State"]["Health"]
                    if health["Status"] != "healthy" or health["FailingStreak"]:
                        failures.append({"service": name, "status": health["Status"],
                                         "failing_streak": health["FailingStreak"]})
                worker = json.loads(stack.run("docker", "inspect", stack.compose("ps", "-q", "aq-worker-cloud").stdout.strip()).stdout)[0]
                if not worker["State"]["Running"] or worker["RestartCount"]:
                    failures.append({"service": "aq-worker-cloud", "state": worker["State"],
                                     "restart_count": worker["RestartCount"]})
                samples += 1
                time.sleep(max(0, 1 - (time.monotonic() - began)))
            elapsed = time.monotonic() - started_at
            used = (slice_usage(cgroup) - started_usage) / 1e6 / elapsed
            assert not failures, failures
            assert samples >= LOAD_SECONDS // 3, samples
            # Sandbox load is real but held to the aggregate quota.
            assert quota * 0.6 <= used <= quota * 1.1, ("sandbox CPU outside quota", used, quota)
            oom = []
            for cid in hogs:
                state = api(sock, "GET", f"/containers/{cid}/json")["State"]
                oom.append({"oom_killed": state.get("OOMKilled"), "exit_code": state.get("ExitCode")})
            assert all(o["oom_killed"] or o["exit_code"] == 137 for o in oom), oom
            evidence["load"] = {
                "seconds": round(elapsed, 1), "sandbox_slice_cpus_used": round(used, 2), "slice_cpu_quota": quota,
                "cpu_sandboxes": SPINNER_SANDBOXES, "spinners_each": SPINNERS_EACH,
                "memory_hogs": oom, "readiness_samples": samples,
                "readiness_max_seconds": round(max(latencies), 2), "readiness_deadline_seconds": READINESS_DEADLINE,
                "health": "postgres, server, aq-graph, aq-git healthy with zero failing streak throughout",
                "host_cpus": os.cpu_count()}
    finally:
        errors = []
        for cid in sandboxes:
            try:
                api(sock, "DELETE", f"/containers/{cid}?force=true", ok=(204, 404))
            except Exception as error:  # report every cleanup failure
                errors.append(str(error))
        systemctl("stop", unit, check=False)
        if unit_file.exists():
            unit_file.unlink()
            systemctl("daemon-reload", check=False)
        assert not errors, errors
    evidence["sandbox_cleanup"] = "sandbox containers and transient slice removed"
    print(json.dumps(evidence, sort_keys=True))
    print("PASS compose_worker_starts_with_docker_local")
    print("PASS sandbox_load_does_not_starve_server_health")


if __name__ == "__main__":
    sandbox_load_does_not_starve_server_health()
