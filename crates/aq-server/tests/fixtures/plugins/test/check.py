#!/usr/bin/python3
"""Exercise the test package through the wire protocol without AQ server."""
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib

PACKAGE = Path(__file__).resolve().parent
PROJECT = "00000000-0000-0000-0000-000000000001"
PRODUCER = "ext.test"
PROTOCOL = "aq.plugin/1"


def invoke(state, secret, mode, key, binding):
    request = {
        "protocol": PROTOCOL,
        "command": f"{PRODUCER}.ping",
        "input": {"mode": mode},
        "idempotency_key": key,
        "project_id": PROJECT,
        "binding": binding,
        "state_dir": str(state),
        "secret_paths": {"plugins/test/secret": str(secret)},
        "deadline_ms": 1000,
    }
    return subprocess.run(
        [str(PACKAGE / "bin/test"), "invoke"],
        input=json.dumps(request).encode(),
        capture_output=True,
        cwd=state,
        env={
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "HOME": str(state),
            "TMPDIR": str(state / "tmp"),
            "AQ_PLUGIN_PROTOCOL": PROTOCOL,
        },
        timeout=request["deadline_ms"] / 1000,
        check=False,
    )


def main():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        state = root / "state"
        (state / "tmp").mkdir(parents=True)
        vault = root / "vault/plugins/test"
        vault.mkdir(parents=True)
        secret = vault / "secret"
        secret.write_text("sample token")
        secret.chmod(0o600)
        bindings = vault / "bindings.toml"
        bindings.write_text(f'[projects.{PROJECT}.options]\nname="fixture"\n')
        bindings.chmod(0o600)
        binding = tomllib.loads(bindings.read_text())["projects"][PROJECT]

        first = invoke(state, secret, "normal", "fixture:normal", binding)
        replay = invoke(state, secret, "normal", "fixture:normal", binding)
        assert first.returncode == replay.returncode == 0
        assert first.stdout == replay.stdout
        assert json.loads(first.stdout) == {
            "outcome": "ok",
            "output": {"environment": ["AQ_PLUGIN_PROTOCOL", "HOME", "PATH", "TMPDIR"]},
            "events": [{
                "name": "ext.test.pinged",
                "payload": {"ref": "ready", "subject": {"id": "sample", "revision": 5}},
            }],
        }

        crash = invoke(state, secret, "crash", "fixture:crash", binding)
        assert crash.returncode != 0
        recovered = invoke(state, secret, "crash", "fixture:crash", binding)
        assert recovered.returncode == 0
        assert recovered.stdout == first.stdout
        assert recovered.stdout == (state / "fixture:crash.json").read_bytes()

        try:
            invoke(state, secret, "timeout", "fixture:timeout", binding)
        except subprocess.TimeoutExpired:
            pass
        else:
            raise AssertionError("first attempt should exceed the child deadline")
        retry = invoke(state, secret, "timeout", "fixture:timeout", binding)
        assert retry.returncode == 0 and retry.stdout == first.stdout

    print("Protocol fixture passed invocation, environment, event payload, crash replay and timeout retry.")


if __name__ == "__main__":
    main()
