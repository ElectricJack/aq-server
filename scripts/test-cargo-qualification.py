"""Check Cargo child environments without compiling the qualified archives."""
import importlib.util
import os
from pathlib import Path
import sys

import pytest


SCRIPTS = Path(__file__).resolve().parent


@pytest.mark.parametrize("script", [
    "test-profile-selection-packages.py",
    "test-central-profile-packages.py",
    "test-admission-refresh-packages.py",
    "test-terminal-refresh-packages.py",
])
def test_qualification_disables_inherited_incremental_and_reuses_slot_target(
    script, monkeypatch, tmp_path,
):
    spec = importlib.util.spec_from_file_location("qualifier", SCRIPTS / script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    work = tmp_path / "extracted"
    work.mkdir()
    target = tmp_path / "leased-slot" / "target"
    target.mkdir(parents=True)
    monkeypatch.setenv("CARGO_TARGET_DIR", str(target))
    monkeypatch.setenv("CARGO_INCREMENTAL", "1")
    monkeypatch.setattr(sys, "argv", [str(SCRIPTS / script)])
    monkeypatch.setattr(module.tempfile, "mkdtemp", lambda **kwargs: str(work))
    children = []

    def run(command, *, cwd, env, check):
        assert check  # A failed Cargo invocation must still fail qualification.
        assert Path(cwd) == work
        children.append((command, env.copy()))

    monkeypatch.setattr(module.subprocess, "run", run)
    module.main()
    assert any(command[:2] == ["cargo", "test"] for command, _ in children)
    assert all(env["CARGO_INCREMENTAL"] == "0" for _, env in children)
    assert all(env["CARGO_TARGET_DIR"] == str(target) for _, env in children)
    assert os.environ["CARGO_INCREMENTAL"] == "1"  # Parent environment is untouched.


def test_profile_package_default_target_stays_in_the_slot(monkeypatch, tmp_path):
    script = SCRIPTS / "test-profile-selection-packages.py"
    spec = importlib.util.spec_from_file_location("profile_qualifier", script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    work = tmp_path / "extracted"
    work.mkdir()
    monkeypatch.delenv("CARGO_TARGET_DIR", raising=False)
    monkeypatch.setattr(module.tempfile, "mkdtemp", lambda **kwargs: str(work))
    children = []
    monkeypatch.setattr(module.subprocess, "run", lambda command, **kwargs: children.append(kwargs))
    module.main()
    assert children
    assert all(child["env"]["CARGO_TARGET_DIR"] == str(module.ROOT / "target") for child in children)
