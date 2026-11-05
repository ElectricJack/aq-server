"""Production/development Compose boundaries and refusal regressions."""
import json
import os
from pathlib import Path
import subprocess
import tomllib

import pytest

ROOT = Path(__file__).resolve().parents[1]
PIN = "example/aq-server@sha256:" + "a" * 64


def config(*overlays):
    env = dict(os.environ, AQ_SERVER_IMAGE=PIN, AQ_VAULT_DIR="/tmp/e1a-vault",
               AQ_UPDATER_SPOOL="/tmp/e1a-spool", AQ_SECRETS_DIR="/tmp/e1a-secrets")
    env.pop("COMPOSE_PROFILES", None)
    command = ["docker", "compose", "-f", str(ROOT / "compose.yaml")]
    for overlay in overlays:
        command += ["-f", str(ROOT / overlay)]
    return json.loads(subprocess.check_output(command + ["config", "--format", "json"], env=env))


def test_production_tunnel_and_vault_preserve_access_boundary():
    document = config()
    services = document["services"]
    for service in services.values():
        assert "@sha256:" in service["image"]
        assert service["read_only"]
        assert service["cap_drop"] == ["ALL"]
        assert not service.get("ports")
        assert "no-new-privileges:true" in service["security_opt"]
    assert services["cloudflared"]["environment"]["TUNNEL_TOKEN_FILE"] == "/run/secrets/tunnel_token"
    assert services["server"]["environment"]["AQ_DEPLOYMENT"] == "production"
    assert document["networks"]["database"]["internal"]
    vault = tomllib.loads((ROOT / "vault.production.example/config.toml").read_text())
    assert vault["environment"] == "production"
    assert vault["auth"]["mode"] == "cloudflare_access"
    assert vault["auth"]["team_domain"].startswith("https://")
    assert vault["auth"]["audience"]
    assert vault["source_retention"]["git_socket"] == "/run/aq-git/central.sock"


def test_development_overlay_excludes_production_credentials_and_initializers():
    services = config("compose.dev.yaml")["services"]
    assert "prod-state-init" not in services and "server-db-init" not in services
    assert services["server"]["environment"]["AQ_DEPLOYMENT"] == "development"
    assert [secret["source"] for secret in services["server"]["secrets"]] == ["postgres_password"]
    assert "prod-state-init" not in services["aq-git"]["depends_on"]
    for service in ("server", "aq-git"):
        sources = {v["target"]: v["source"] for v in services[service]["volumes"]}
        assert sources["/var/lib/aq"].endswith("dev_source")
        assert sources["/run/aq-git"].endswith("dev_git_run")


@pytest.mark.parametrize("image", ["server:latest", "server@sha256:abc", "server@sha256:" + "z" * 64])
def test_production_initializer_refuses_mutable_or_malformed_image_before_state_changes(image):
    result = subprocess.run(["sh", str(ROOT / "scripts/prod-init-volumes.sh")],
                            env=dict(os.environ, AQ_SERVER_IMAGE=image), capture_output=True, text=True)
    assert result.returncode != 0
    assert "digest" in result.stderr


def test_database_initializer_refuses_unknown_owner_before_connecting():
    result = subprocess.run(["sh", str(ROOT / "scripts/prod-db-init.sh"), "postgres", "postgres_password"],
                            capture_output=True, text=True)
    assert result.returncode != 0
    assert "unsupported database owner" in result.stderr


def test_product_foundation_keeps_its_standalone_database_fixture(tmp_path):
    import importlib.util
    spec = importlib.util.spec_from_file_location("product_smoke", ROOT / "scripts/product-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    override = tmp_path / "foundation.yaml"
    override.write_text(module.FOUNDATION_OVERRIDE)
    services = config(override)["services"]
    assert set(services) == {"server", "postgres", "cloudflared"}
    assert set(services["server"]["depends_on"]) == {"postgres"}
    assert [secret["source"] for secret in services["server"]["secrets"]] == ["postgres_password"]


def test_published_graph_reads_private_token_from_initialized_shared_runtime():
    services = config()["services"]
    graph = services["aq-graph"]
    assert graph["environment"]["AQ_GRAPH_BOOTSTRAP_TOKEN_FILE"] == "/run/aq/graph_token"
    assert graph["depends_on"]["prod-state-init"]["condition"] == "service_completed_successfully"
    runtime = [{v["target"]: v["source"] for v in services[name]["volumes"]}
               for name in ("server", "aq-graph", "prod-state-init")]
    assert len({v["/run/aq"] for v in runtime}) == 1


def test_worker_overlay_limits_and_socket_boundaries():
    import importlib.util
    spec = importlib.util.spec_from_file_location("workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    report = module.validate_production(module.merged_config(), module.slice_properties())
    assert report["budget"]["cpus"] <= 4.0
    assert report["budget"]["memory_bytes"] <= 8_000_000_000 - 1_000_000_000
    # The base stack stays the E1a service set; workers come only from the overlay.
    assert not {"aq-supervisor-host", "aq-worker-cloud"} & set(config()["services"])


def test_boat_profile_has_distinct_state_and_requires_additional_host_capacity():
    import importlib.util
    spec = importlib.util.spec_from_file_location("workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert "aq-worker-boat" not in module.merged_config()["services"]
    document = module.merged_config("boat")
    with pytest.raises(AssertionError, match="CPU budget exceeds VPS"):
        module.validate_production(document, module.slice_properties())
    report = module.validate_production(document, module.slice_properties(),
                                        {"cpus": 8.0, "memory": 16_000_000_000})
    assert report["budget"]["cpus"] == 4.125
    example = json.loads((ROOT / "deploy/boat.example/controller.json.example").read_text())
    assert "docker_local" not in example
    assert example["boat"]["attempt_org"] != example["boat"]["template_org"]
    assert example["node"]["max_concurrent_sandboxes"] == 4
    assert example["node"]["spend_ceiling"] == {"currency": "USD", "micros": 18000000}


@pytest.mark.parametrize("key,value,message", [
    ("CPUQuota", "400%", "CPU budget exceeds VPS"),
    ("MemoryMax", "8G", "memory budget leaves under 1 GB for the host"),
])
def test_worker_budget_rejects_previous_large_vps_slice(key, value, message):
    import importlib.util
    spec = importlib.util.spec_from_file_location("workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    properties = module.slice_properties()
    properties[key] = value
    with pytest.raises(AssertionError, match=message):
        module.validate_production(module.merged_config(), properties)


@pytest.mark.parametrize("uid,gid", [("0", "1000"), ("1000", "0"), ("bad", "1000"), ("1000", "")])
def test_worker_initializer_refuses_invalid_identity_before_changing_volumes(uid, gid):
    result = subprocess.run(["sh", str(ROOT / "scripts/worker-init-volumes.sh")],
                            env=dict(os.environ, AQ_SANDBOX_UID=uid, AQ_SANDBOX_GID=gid),
                            capture_output=True, text=True)
    assert result.returncode != 0
    assert "sandbox UID/GID must be" in result.stderr


@pytest.mark.parametrize("image", ["worker:latest", "worker@sha256:abc", ""])
def test_worker_smoke_refuses_unpinned_worker_before_starting_stack(monkeypatch, tmp_path, image):
    import importlib.util
    spec = importlib.util.spec_from_file_location("workers_smoke", ROOT / "scripts/compose-workers-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    monkeypatch.setenv("AQ_WORKER_IMAGE", image)
    with pytest.raises(AssertionError, match="published Dockerfile.worker digest"):
        module.worker_setup(str(tmp_path / "unavailable.sock"), {})
