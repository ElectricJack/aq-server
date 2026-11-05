"""Build the real image, then exercise native UID/stdin/credential separation.
No provider request or real credential is used; this is not Codex qualification.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[3]
IMAGE = "aq-supervisor-host:f1a-steady-harbor-87-1"
def raw(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
def digest(value):
    return hashlib.sha256(raw(value)).hexdigest()
def pin(name):
    return dict(id=name, version="1", digest=hashlib.sha256(name.encode()).hexdigest())

subprocess.run(["docker", "build", "--progress=plain", "-f", "Dockerfile.supervisor-host", "-t", IMAGE, "."], cwd=ROOT, check=True)
central = json.loads((ROOT / "crates/aq-central-authority/artifacts/examples.json").read_text())["plan"]["data"]
legacy = json.loads((ROOT / "vendor/aq-session-host-0.1.6/tests/fixtures/manifest.json").read_text())
now = int(time.clock_gettime(time.CLOCK_BOOTTIME) * 1000)
central["wall_time_ms"] = 60000
central["max_permit_ms"] = 30000
legacy["harness"].update(harness="codex", cli_version="0.166.0")
legacy["terminal"]["input_enabled"] = False
legacy["resources"]["wall_time_ms"] = 60000
startup = b"fixture startup only\n"
subject = {key: central["subject"][key] for key in ("kind", "actor_id", "conversation_id", "conversation_revision")}
host = dict(host_plan_version=1, subject=subject, session=central["session"], policy=central["subject"]["policy"], execution=central["profiles"]["execution"], input=dict(dispatch_id=central["dispatch_id"], host_operation_id=central["host_operation_id"], batch={**pin("batch"), "digest": digest(central["batch"])}), harness=legacy["harness"], resources=legacy["resources"], terminal=legacy["terminal"], startup=dict(descriptor=pin("startup"), payload={**pin("payload"), "digest": hashlib.sha256(startup).hexdigest()}, bytes=len(startup)), machine_boot_id=Path("/proc/sys/kernel/random/boot_id").read_text().strip(), admitted_boot_ms=now, deadline_boot_ms=now+60000)
central["profiles"].update(host_plan={**pin("host"), "digest": digest(host)}, adapter=host["harness"]["adapter"], environment=host["harness"]["environment"], startup=host["startup"]["descriptor"])
native = b'''#!/bin/sh
set -eu
[ "$(id -u)" = 10001 ]
[ "${OPENAI_API_KEY-unset}" = unset ]
if cat /secrets/provider >/dev/null 2>&1; then exit 99; fi
cat
printf 'native-boundary-ok\\n'
'''
launch = str(uuid.uuid4())
config = dict(database_url="postgres://unused", root="/host", workspace="/workspace", helper="/usr/local/bin/aq-session-host", tmux="/usr/bin/tmux", tmux_version="unused", native="/tmp/native", native_digest=hashlib.sha256(native).hexdigest(), homes="/native-homes", secret_file="/secrets/provider", model=dict(provider="openai", model="fixture-model", reasoning="high", max_output_tokens=100, spending_budget=None), central=central, host=host, startup_file="/tmp/startup", resource_admission=pin("fixture-admission"))
with tempfile.TemporaryDirectory(prefix="f1a-image-") as name:
    directory=Path(name)
    for filename, contents in [("config", raw(config)), ("startup", startup), ("native", native)]:
        (directory/filename).write_bytes(contents)
    script = '''set -eu
mkdir -p /secrets /host/"$1"
printf fixture-provider-never-disclosed > /secrets/provider
chmod 600 /secrets/provider
cp /fixture/config /tmp/config
cp /fixture/native /tmp/native
cp /fixture/startup /host/"$1"/context-0.bin
chmod 600 /tmp/config /host/"$1"/context-0.bin
chmod 755 /tmp/native
export AQ_SESSION_DIRECTORY=/host/"$1"
exec /usr/local/bin/aq-supervisor-host native /tmp/config "$2"
'''
    result=subprocess.run(["docker", "run", "--rm", "--network", "none", "--mount", f"type=bind,src={directory},dst=/fixture,readonly", "--entrypoint", "/bin/sh", IMAGE, "-c", script, "fixture", launch, digest(config)], capture_output=True, timeout=90)
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
    assert result.stdout == startup+b"native-boundary-ok\n", result.stdout
    assert b"fixture-provider-never-disclosed" not in result.stdout+result.stderr
print("Image built; native UID 10001 received exact stdin and could not read provider placement; no provider request made.")
