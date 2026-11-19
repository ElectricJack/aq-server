"""ms1-repin-project.py --apply must prove the server was recreated, using a fake `docker` on PATH."""
import importlib.util
import os
import stat
import textwrap
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("repin", Path(__file__).with_name("ms1-repin-project.py"))
repin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(repin)

PROJECT = "e0000000-0000-4000-8000-000000000002"
OLD = "a" * 40
NEW = "b" * 40
CONFIG = f"""[profiled_execution]
revision = 3

[[profiled_execution.projects]]
project_id = "{PROJECT}"
retention_handle = "handle-old"

[profiled_execution.projects.source]
kind = "immutable"

[profiled_execution.projects.source.object]
algorithm = "sha1"
hex = "{OLD}"
"""

# State lives in files under $FAKE_DOCKER: `started` (the container's start time), `config`, `healthy`.
FAKE_DOCKER = textwrap.dedent('''\
    #!/usr/bin/env python3
    import os, sys
    state = os.environ["FAKE_DOCKER"]
    read = lambda name: open(os.path.join(state, name)).read()
    args = sys.argv[1:]
    if args[:1] == ["inspect"] and "{{.Config.Image}}" in args:
        print("aq-server:test")
    elif args[:1] == ["inspect"]:
        print("container-1 " + read("started").strip() + " true")
    elif args[:1] == ["exec"] and "cat" in args:
        sys.stdout.write(read("config"))
    elif args[:1] == ["exec"] and any("psql" in a for a in args):
        print("e0000000-0000-4000-8000-000000000003")
    elif args[:1] == ["exec"] and args[-2:] == ["aq-server", "healthcheck"]:
        sys.exit(0 if read("healthy").strip() == "yes" else 1)
    elif args[:1] == ["run"]:
        open(os.path.join(state, "config"), "w").write(sys.stdin.read())
    else:
        sys.exit("fake docker: unexpected " + repr(args))
''')


@pytest.fixture
def stack(tmp_path, monkeypatch):
    state = tmp_path / "state"
    state.mkdir()
    (state / "started").write_text("2026-11-19T01:00:00Z")
    (state / "config").write_text(CONFIG)
    (state / "healthy").write_text("yes")
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    docker = bin_dir / "docker"
    docker.write_text(FAKE_DOCKER)
    docker.chmod(docker.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("FAKE_DOCKER", str(state))
    monkeypatch.setattr(repin, "retain_main", lambda repository, tag: (NEW, "handle-new"))
    return state


def run(monkeypatch, *argv):
    monkeypatch.setattr("sys.argv", ["ms1-repin-project.py", PROJECT, "--apply", "--wait-seconds", "1",
                                     "--poll-seconds", "0.1", *argv])
    return repin.main()


def test_a_compose_command_that_does_nothing_fails(stack, monkeypatch):
    with pytest.raises(SystemExit) as failed:
        run(monkeypatch, "--compose", "true")
    assert "the server was not recreated; the new pin is written but not loaded" in str(failed.value)
    assert NEW in (stack / "config").read_text()  # the pin was written, which is why the message says so


def test_a_compose_command_that_recreates_the_server_passes(stack, monkeypatch):
    recreate = f"echo 2026-11-19T02:00:00Z > {stack}/started"
    assert run(monkeypatch, "--compose", recreate) == 0
    assert NEW in (stack / "config").read_text()


def test_a_recreated_server_that_is_not_healthy_fails(stack, monkeypatch):
    recreate = f"echo 2026-11-19T02:00:00Z > {stack}/started; echo no > {stack}/healthy"
    with pytest.raises(SystemExit) as failed:
        run(monkeypatch, "--compose", recreate)
    assert "the server was not recreated" in str(failed.value)


def test_a_failing_compose_command_keeps_its_exit_code(stack, monkeypatch):
    assert run(monkeypatch, "--compose", "exit 7") == 7


def test_help_documents_the_compose_form(capsys, monkeypatch):
    monkeypatch.setattr("sys.argv", ["ms1-repin-project.py", "--help"])
    with pytest.raises(SystemExit):
        repin.main()
    out = "".join(capsys.readouterr().out.split())  # argparse wraps at hyphens
    assert "--compose'compose.shafterup-d--no-deps--no-build--force-recreate--waitserver'" in out
    assert "--wait-seconds" in out
