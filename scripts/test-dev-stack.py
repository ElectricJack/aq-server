"""Dev bootstrap regression tests: exercise the command and its persisted state."""
import json
import os
from pathlib import Path
import subprocess
import importlib.util
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

ROOT = Path(__file__).resolve().parents[1]


def command(tmp_path, *args, extra=None):
    env = dict(os.environ, AQ_DEV_STATE_DIR=str(tmp_path / "state"),
               AQ_SECRETS_DIR=str(tmp_path / "secrets"))
    for key in ("AQ_VAULT_DIR", "DEV_SCRATCH_REPO_URL", "DEV_SCRATCH_BASE_BRANCH",
                "DEV_SCRATCH_CREDENTIAL_REF", "AQ_DEV_SERVER_URL", "AQ_DEV_ADMIN_PORT",
                "AQ_DEV_INTELLIGENCE_FILE", "AQ_DEV_REPLACE_OPERATOR_SECTION"):
        env.pop(key, None)
    env.update(extra or {})
    return subprocess.run(["python3", str(ROOT / "scripts/dev-stack.py"), *args],
                          env=env, text=True, capture_output=True)


def test_prepare_replay_preserves_private_credentials_and_config(tmp_path):
    first = command(tmp_path, "prepare")
    assert first.returncode == 0, first.stderr
    env = json.loads(first.stdout)
    token = tmp_path / "secrets/graph_token"
    original = token.read_bytes()
    assert len(original.strip()) >= 32
    assert token.stat().st_mode & 0o777 == 0o600
    assert original not in first.stdout.encode()
    assert command(tmp_path, "prepare").returncode == 0
    assert token.read_bytes() == original
    vault = Path(env["AQ_VAULT_DIR"]) / "config.toml"
    import tomllib
    config = tomllib.loads(vault.read_text())
    assert config["auth"]["mode"] == "dev"
    assert config["agent_access"]["graph_url"] == "http://aq-graph:8741"
    assert config["source_retention"]["git_socket"] == "/run/aq-git/central.sock"
    # Terminal watch/control need a protection keyring to mount on the dev stack.
    protection = config["terminals"]["protection"]
    assert protection["active_key_id"] == protection["keys"][0]["id"]
    assert protection["keys"][0]["file"] == "/run/aq-dev/terminal_protection_key"
    assert len((tmp_path / "secrets/terminal_protection_key").read_bytes()) == 32
    init = (ROOT / "scripts/dev-init-volumes.sh").read_text()
    assert "/run/secrets/terminal_protection_key /run/aq-dev/terminal_protection_key" in init
    assert "terminal_protection_key:" in (ROOT / "compose.dev.yaml").read_text()
    assert command(tmp_path, "prepare").stdout == first.stdout


@pytest.mark.parametrize("name", ["playbook_database_password", "playbook_engine_keys", "playbook_bridge_key",
                                  "terminal_protection_key"])
def test_settlement_credentials_are_private_retained_and_refuse_symlinks(tmp_path, name):
    first = command(tmp_path, "prepare")
    assert first.returncode == 0, first.stderr
    path = tmp_path / "secrets" / name
    original = path.read_bytes()
    assert path.stat().st_mode & 0o777 == 0o600
    assert command(tmp_path, "prepare").returncode == 0
    assert path.read_bytes() == original
    path.unlink()
    other = tmp_path / "retained-secret"
    other.write_bytes(original)
    path.symlink_to(other)
    refused = command(tmp_path, "prepare")
    assert refused.returncode != 0 and "unsafe retained" in refused.stderr
    assert other.read_bytes() == original


def test_settlement_requires_seed_before_contacting_services(tmp_path):
    result = command(tmp_path, "settlement")
    assert result.returncode != 0 and "completed development seed" in result.stderr


def test_settlement_restart_preserves_grant_deadline_keys_and_namespace(tmp_path, monkeypatch):
    import tomllib
    dev = load_dev()
    monkeypatch.setenv("AQ_DEV_STATE_DIR", str(tmp_path / "state"))
    monkeypatch.setenv("AQ_SECRETS_DIR", str(tmp_path / "secrets"))
    monkeypatch.delenv("AQ_VAULT_DIR", raising=False)
    dev.prepare()
    state, secret, vault = dev.paths()
    (state / "seed.json").write_text(json.dumps(dict(project_id=dev.PROJECT)))
    owner = "e0000000-0000-4000-8000-000000000099"
    calls = []

    def compose(env, *args):
        calls.append(args)
        return "999" if args[0] == "exec" else ""

    def http(url):
        if url.endswith("/api/users"):
            return [dict(email="owner@localhost", role="owner", bound=True)]
        if url.endswith("/api/v1/bootstrap"):
            return dict(data=dict(principal=dict(id=owner)))
        if "/api/projects/" in url:
            return dict(binding=dict(enabled=True, graph_workspace_id=dev.WORKSPACE))
        return dict(namespace=dict(namespace_id=json.loads((state / "settlement.json").read_text())["namespace_id"]))

    monkeypatch.setattr(dev, "compose", compose)
    monkeypatch.setattr(dev, "http", http)
    first = dev.enable_settlement()
    before = {p: p.read_bytes() for p in (vault / "playbooks.toml", secret / "playbook_bridge_key",
        secret / "playbook_engine_keys", state / "settlement.json")}
    monkeypatch.setattr(dev.time, "time", lambda: 1)
    assert dev.enable_settlement() == first
    assert all(p.read_bytes() == content for p, content in before.items())
    config = tomllib.loads((vault / "playbooks.toml").read_text())["playbooks"]
    assert config["profile"] == "human_review_v2" and config["socket_owner_uid"] == 999
    assert config["namespace"]["owner_user_id"] == owner
    assert config["deployments"][0]["scope"]["capabilities"] == ["operation.get"]
    assert calls.index(("stop", "server")) < calls.index(("run", "--rm", "--no-deps", "dev-state-init"))
    pins = json.loads((state / "settlement.json").read_text())
    pins["owner_user_id"] = dev.TASK
    (state / "settlement.json").write_text(json.dumps(pins))
    calls.clear()
    with pytest.raises(ValueError, match="owner/socket differs"):
        dev.enable_settlement()
    assert all(call[0] == "exec" for call in calls), "refusal restarted the server"


def test_seed_unset_or_partial_designation_fails_before_contacting_services(tmp_path):
    for extra in ({}, {"DEV_SCRATCH_REPO_URL": "https://github.com/example/scratch.git"}):
        result = command(tmp_path, "seed", extra=extra)
        assert result.returncode != 0
        assert "DEV_SCRATCH" in result.stderr
        assert "Traceback" not in result.stderr
        assert not (tmp_path / "state/seed.json").exists()


def test_seed_rejects_credentials_in_url_and_unsupported_credential_profiles(tmp_path):
    for url, credential in (("https://user:secret@github.com/owner/scratch.git", "none"),
                            ("https://github.com/owner/scratch.git?token=secret", "none"),
                            ("https://github.com/owner/scratch.git", "private-profile")):
        result = command(tmp_path, "seed", extra=dict(DEV_SCRATCH_REPO_URL=url,
            DEV_SCRATCH_BASE_BRANCH="main", DEV_SCRATCH_CREDENTIAL_REF=credential))
        assert result.returncode != 0
        assert "DEV_SCRATCH" in result.stderr
        assert "secret" not in result.stderr
        assert not (tmp_path / "state/seed.json").exists()


def test_seed_refuses_retained_credentials_after_database_reset_before_enrolling(tmp_path):
    assert command(tmp_path, "prepare").returncode == 0
    credential = tmp_path / "secrets/dev_worker.json"
    original = b'{"node_id":"old-node","credential":"aqn_retained-secret"}'
    credential.write_bytes(original)
    posts = []

    class EmptyWorkerDatabase(BaseHTTPRequestHandler):
        def do_GET(self):
            assert self.path == "/api/workers"
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"[]")

        def do_POST(self):
            posts.append(self.path)
            self.send_response(500)
            self.end_headers()

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), EmptyWorkerDatabase)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        result = command(tmp_path, "seed", extra=dict(
            DEV_SCRATCH_REPO_URL="https://github.com/example/scratch.git", DEV_SCRATCH_BASE_BRANCH="main",
            DEV_SCRATCH_CREDENTIAL_REF="none", AQ_DEV_SERVER_URL=f"http://127.0.0.1:{server.server_port}"))
        assert result.returncode != 0
        assert "worker credential has no matching worker" in result.stderr
        assert not posts, "seed enrolled another node before refusing the stale credential"
        assert credential.read_bytes() == original
        assert "retained-secret" not in result.stderr
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def load_dev():
    spec = importlib.util.spec_from_file_location("e0_dev_stack", ROOT / "scripts/dev-stack.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_interrupted_credential_write_never_publishes_a_partial_record(tmp_path, monkeypatch):
    dev = load_dev()
    credential = tmp_path / "dev_worker.json"

    def interrupted_write(record, output):
        output.write('{"credential":')
        raise OSError("simulated disk failure")

    monkeypatch.setattr(dev.json, "dump", interrupted_write)
    with pytest.raises(OSError, match="disk failure"):
        dev.save_worker_credential(credential, {})
    assert not credential.exists()
    assert list(tmp_path.iterdir()) == []


def test_atomic_credential_publish_preserves_an_existing_record(tmp_path):
    dev = load_dev()
    credential = tmp_path / "dev_worker.json"
    dev.save_worker_credential(credential, {"credential": "first"})
    assert credential.stat().st_mode & 0o777 == 0o600
    with pytest.raises(FileExistsError):
        dev.save_worker_credential(credential, {"credential": "replacement"})
    assert json.loads(credential.read_text()) == {"credential": "first"}
    assert list(tmp_path.iterdir()) == [credential]


def test_prepare_appends_only_an_operator_intelligence_section(tmp_path):
    """MS1: an opt-in [intelligence] file (no secrets) joins the generated vault."""
    import tomllib
    section = tmp_path / "intelligence.toml"
    section.write_text('[intelligence]\nrevision = 1\nmaterial_store = "/var/lib/aq/login-material"\n')
    extra = dict(AQ_DEV_INTELLIGENCE_FILE=str(section))
    first = command(tmp_path, "prepare", extra=extra)
    assert first.returncode == 0, first.stderr
    vault = Path(json.loads(first.stdout)["AQ_VAULT_DIR"]) / "config.toml"
    config = tomllib.loads(vault.read_text())
    assert config["intelligence"]["material_store"] == "/var/lib/aq/login-material"
    assert config["auth"]["mode"] == "dev"
    # Replay with the same file is stable; dropping or changing it is refused.
    assert command(tmp_path, "prepare", extra=extra).returncode == 0
    assert command(tmp_path, "prepare").returncode != 0
    for bad in ('[server]\nadmin_bind = "0.0.0.0:1"\n', '[intelligence]\nrevision = 1\nsecret = "x"\n[auth]\nmode = "x"\n',
                "not toml ["):
        other = tmp_path / "bad.toml"
        other.write_text(bad)
        refused = command(tmp_path / "fresh", "prepare", extra=dict(AQ_DEV_INTELLIGENCE_FILE=str(other)))
        assert refused.returncode != 0
        assert "intelligence" in refused.stderr
    link = tmp_path / "link.toml"
    link.symlink_to(section)
    assert command(tmp_path / "linked", "prepare", extra=dict(AQ_DEV_INTELLIGENCE_FILE=str(link))).returncode != 0


def test_operator_section_may_carry_profiled_execution_and_be_replaced_explicitly(tmp_path):
    """MS1: [profiled_execution] may join [intelligence]; replacing the operator
    section of an existing vault needs AQ_DEV_REPLACE_OPERATOR_SECTION=1 and
    never touches a generated table."""
    import tomllib
    section = tmp_path / "intelligence.toml"
    section.write_text('[intelligence]\nrevision = 1\n')
    extra = dict(AQ_DEV_INTELLIGENCE_FILE=str(section))
    first = command(tmp_path, "prepare", extra=extra)
    assert first.returncode == 0, first.stderr
    vault = Path(json.loads(first.stdout)["AQ_VAULT_DIR"]) / "config.toml"
    section.write_text('[intelligence]\nrevision = 2\n[profiled_execution]\nrevision = 1\nqualified_runtimes = []\nprojects = []\n')
    assert command(tmp_path, "prepare", extra=extra).returncode != 0
    replaced = command(tmp_path, "prepare", extra=dict(extra, AQ_DEV_REPLACE_OPERATOR_SECTION="1"))
    assert replaced.returncode == 0, replaced.stderr
    config = tomllib.loads(vault.read_text())
    assert config["intelligence"]["revision"] == 2 and config["profiled_execution"]["revision"] == 1
    assert command(tmp_path, "prepare", extra=extra).returncode == 0
    vault.write_text(vault.read_text().replace('mode = "dev"', 'mode = "other"'))
    assert command(tmp_path, "prepare", extra=dict(extra, AQ_DEV_REPLACE_OPERATOR_SECTION="1")).returncode != 0
    section.write_text('[profiled_execution]\nrevision = 1\n')
    assert command(tmp_path / "fresh", "prepare", extra=extra).returncode != 0


def test_public_origin_follows_the_published_admin_url_in_both_vault_copies(tmp_path):
    """QA 2026-11-06 section 1: vault.example's 127.0.0.1:8080 origin made the CSRF
    guard refuse every browser write on a stack published on another port."""
    import tomllib

    def origin(path):
        return tomllib.loads(path.read_text())["server"]["public_origin"]

    first = command(tmp_path, "prepare")
    assert first.returncode == 0, first.stderr
    vault = Path(json.loads(first.stdout)["AQ_VAULT_DIR"])
    assert origin(vault / "config.toml") == "http://127.0.0.1:8080"
    port = command(tmp_path / "port", "prepare", extra=dict(AQ_DEV_ADMIN_PORT="38080"))
    assert port.returncode == 0, port.stderr
    assert origin(Path(json.loads(port.stdout)["AQ_VAULT_DIR"]) / "config.toml") == "http://127.0.0.1:38080"
    published = dict(AQ_DEV_SERVER_URL="http://127.0.0.1:28080/")
    for bad in ("ftp://127.0.0.1:28080", "http://127.0.0.1:28080/app", "http://u:p@127.0.0.1:1",
                "http://127.0.0.1:x"):
        refused = command(tmp_path / "bad", "prepare", extra=dict(AQ_DEV_SERVER_URL=bad))
        assert refused.returncode != 0 and "AQ_DEV_SERVER_URL" in refused.stderr, bad
    # The settlement copy (/vault/settlement/config.toml in the volume) is
    # config.toml plus [playbooks]; an origin change rewrites both, explicitly.
    (vault / "playbooks.toml").write_text((vault / "config.toml").read_text() + "[playbooks]\nenabled = true\n")
    # A replay keeps the first origin (smoke stacks learn ephemeral ports later).
    assert command(tmp_path, "prepare", extra=published).returncode == 0
    assert origin(vault / "config.toml") == "http://127.0.0.1:8080"
    replaced = command(tmp_path, "prepare", extra=dict(published, AQ_DEV_REPLACE_OPERATOR_SECTION="1"))
    assert replaced.returncode == 0, replaced.stderr
    assert origin(vault / "config.toml") == "http://127.0.0.1:28080"
    settlement = tomllib.loads((vault / "playbooks.toml").read_text())
    assert settlement["server"]["public_origin"] == "http://127.0.0.1:28080"
    assert settlement["playbooks"] == {"enabled": True}
    assert command(tmp_path, "prepare", extra=published).returncode == 0


def test_quota_sources_replace_operator_tables_without_changing_generated_keys(tmp_path):
    import tomllib
    section = tmp_path / "operator.toml"
    section.write_text('intelligence_preferences_file = "/var/lib/aq/models.json"\n[intelligence]\nrevision = 1\n')
    extra = dict(AQ_DEV_INTELLIGENCE_FILE=str(section))
    first = command(tmp_path, "prepare", extra=extra)
    assert first.returncode == 0, first.stderr
    vault = Path(json.loads(first.stdout)["AQ_VAULT_DIR"]) / "config.toml"
    original = tomllib.loads(vault.read_text())
    with section.open("a") as output:
        output.write('\n[[provider_quota]]\nprovider_id = "openai"\naccount_ref = "Codex"\nsnapshot_file = "/var/lib/aq/usage.json"\n')
    updated = command(tmp_path, "prepare", extra=dict(extra, AQ_DEV_REPLACE_OPERATOR_SECTION="1"))
    assert updated.returncode == 0, updated.stderr
    actual = tomllib.loads(vault.read_text())
    assert actual.pop("provider_quota")[0]["snapshot_file"] == "/var/lib/aq/usage.json"
    assert actual == original
