#!/usr/bin/env python3
"""E1d: real Boat service starts without credentials; no Boat API is called."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
workers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workers)


def boat_configuration_required():
    image = os.environ.get("AQ_BOAT_WORKER_IMAGE") or os.environ.get("AQ_WORKER_IMAGE")
    if not image:
        image = next(line.split("=", 1)[1] for line in (ROOT / "deploy/worker-image.env").read_text().splitlines()
                     if line.startswith("AQ_WORKER_IMAGE="))
    assert workers.prod.DIGEST.fullmatch(image), "worker image must be digest-pinned"
    project = "aq-e1d-" + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix=project) as temporary:
        directory = Path(temporary)
        (directory / "config").mkdir()
        (directory / "empty-key").touch()
        env = dict(os.environ, **workers.PLACEHOLDERS)
        env.update(AQ_BOAT_WORKER_IMAGE=image, AQ_BOAT_ENABLED="0",
                   AQ_BOAT_CONFIG_DIR=str(directory / "config"),
                   AQ_BOAT_ATTEMPT_KEY_FILE=str(directory / "empty-key"))
        for key in ("COMPOSE_FILE", "COMPOSE_PROFILES"):
            env.pop(key, None)

        def compose(*args):
            return subprocess.check_output(["docker", "compose", "-p", project,
                "-f", str(ROOT / "compose.yaml"), "-f", str(ROOT / "compose.workers.yaml"),
                "--profile", "boat", *args],
                cwd=ROOT, env=env, text=True, timeout=120)

        def command(*args):
            return compose("exec", "-T", "aq-worker-boat", *args).strip()

        config = json.loads(compose("config", "--format", "json"))
        # The Boat rollout starts on the desktop. Keep the existing VPS budget
        # strict and check this host's actual capacity for the expanded stack.
        cpu, memory = subprocess.check_output(
            ["docker", "info", "--format", "{{.NCPU}} {{.MemTotal}}"], text=True).split()
        host_budget = {"cpus": int(cpu), "memory": int(memory)}
        production = workers.validate_production(config, workers.slice_properties(), host_budget)
        try:
            compose("up", "-d", "--no-build", "aq-worker-boat")
            cid = compose("ps", "-q", "aq-worker-boat").strip()
            deadline = time.monotonic() + 15
            while True:
                container = json.loads(subprocess.check_output(["docker", "inspect", cid], text=True))[0]
                if container["State"].get("Health", {}).get("Status") == "healthy":
                    break
                assert time.monotonic() < deadline, "Boat placeholder did not become healthy"
                time.sleep(0.1)
            assert command("cat", "/run/aq-boat/status") == "configuration-required"
            assert "configuration-required" in compose("logs", "--no-color", "aq-worker-boat")
            assert command("id", "-u") == "10001"
            assert command("stat", "-c", "%u:%g:%a", "/state", "/var/lib/aq-worker/leases").splitlines() == ["10001:10001:700"] * 2
            command("sh", "-ec", 'test ! -e /run/aq-boat/api-key')
            host = container["HostConfig"]
            limits = production["service_limits"]["aq-worker-boat"]
            assert host["NanoCpus"] == int(limits["cpus"] * 1e9)
            assert host["Memory"] == host["MemorySwap"] == limits["memory"]
            assert host["PidsLimit"] == limits["pids"]
            # Both controller and mapping files are private, on this node's
            # persistent volume. A restart must preserve them, not reinitialize.
            command("sh", "-ec", 'umask 077; mkdir -p /state/controller; echo smoke > /state/controller/smoke; echo smoke > /state/boat-mappings.smoke')
            compose("restart", "aq-worker-boat")
            assert command("cat", "/state/controller/smoke", "/state/boat-mappings.smoke") == "smoke\nsmoke"
            assert command("cat", "/run/aq-boat/status") == "configuration-required"
            evidence = {"test": "boat_configuration_required", "image": image,
                        "state": "configuration-required", "persistent_state": "survived restart",
                        "limits": limits, "host_budget": host_budget,
                        "boat_api": "not called", "enrollment": "not performed"}
        finally:
            compose("down", "--volumes", "--remove-orphans")
            assert not compose("ps", "-aq").strip(), "owned containers survived cleanup"
        print(json.dumps(dict(evidence, cleanup="owned containers and volumes removed"), sort_keys=True))
        print("PASS boat_configuration_required")


if __name__ == "__main__":
    boat_configuration_required()
