#!/usr/bin/env python3
"""Run the loopback browser pilot with a throwaway PostgreSQL and tmux pane."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import tempfile
import time
from uuid import UUID

from websockets.asyncio.client import connect


def run(*command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


class Pane:
    def __init__(self, directory, shell_container):
        self.tmux = ("tmux", "-f", "/dev/null", "-S", str(directory / "tmux"))
        self.log = directory / "pane.log"
        self.log.touch()
        # Docker runs the shell, so browser input cannot execute on the host.
        shell = "printf 'AQ_TERMINAL_PILOT_READY\\n'; exec sh -i"
        command = shlex.join(("docker", "exec", "-it", "--user", "postgres",
                              shell_container, "sh", "-c", shell))
        run(*self.tmux, "new-session", "-d", "-x", "120", "-y", "40", command)
        run(*self.tmux, "pipe-pane", "cat >> " + shlex.quote(str(self.log)))

    def screen(self):
        return run(*self.tmux, "capture-pane", "-p", "-e", "-S", "-100",
                   capture_output=True).stdout.replace(b"\n", b"\r\n")

    def input(self, data):
        run(*self.tmux, "send-keys", "-H", *(f"{byte:02x}" for byte in data))

    def resize(self, frame):
        run(*self.tmux, "resize-window", "-x", str(frame["columns"]),
            "-y", str(frame["rows"]))

    def size(self):
        result = run(*self.tmux, "display-message", "-p", "#{pane_width} #{pane_height}",
                     capture_output=True)
        return tuple(map(int, result.stdout.split()))

    def close(self):
        subprocess.run((*self.tmux, "kill-server"), check=False)


async def relay(socket, pane):
    viewers = {}
    with pane.log.open("rb") as output:
        output.seek(0, os.SEEK_END)
        incoming = asyncio.create_task(socket.recv())
        try:
            while True:
                received, _ = await asyncio.wait({incoming}, timeout=0.04)
                if received:
                    frame = json.loads(incoming.result())
                    incoming = asyncio.create_task(socket.recv())
                    identity = frame["connection_id"]
                    kind = frame["kind"]
                    if kind == "attach":
                        viewers[identity] = frame.get("read_only", False)
                    if kind in ("attach", "repaint"):
                        width, height = pane.size()
                        await socket.send(json.dumps({
                            "kind": "ready", "connection_id": identity,
                            "columns": width, "rows": height,
                        }))
                        await socket.send(UUID(identity).bytes + pane.screen())
                    elif kind == "detach":
                        viewers.pop(identity, None)
                    elif kind == "input" and viewers.get(identity) is False:
                        pane.input(frame["bytes"])
                    elif kind == "resize" and viewers.get(identity) is False:
                        pane.resize(frame)
                chunk = output.read(4096)
                if chunk:
                    for identity in viewers:
                        await socket.send(UUID(identity).bytes + chunk)
        finally:
            incoming.cancel()


def await_peer(server, directory):
    manifest = directory / "peer.json"
    while server.poll() is None:
        if manifest.exists():
            return json.loads(manifest.read_text())
        time.sleep(0.1)
    raise RuntimeError(f"pilot test exited with code {server.returncode}")


def start_database(name, password):
    run("docker", "run", "--rm", "-d", "--name", name,
        "-e", f"POSTGRES_PASSWORD={password}", "-p", "127.0.0.1::5432",
        "postgres:17-bookworm", "-c", "max_locks_per_transaction=512",
        stdout=subprocess.DEVNULL)
    host_port = run("docker", "port", name, "5432/tcp",
                    capture_output=True, text=True).stdout.strip().rsplit(":", 1)[1]
    for _ in range(100):
        ready = subprocess.run(
            ("docker", "exec", name, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
        )
        if ready.returncode == 0:
            return f"postgresql://postgres:{password}@127.0.0.1:{host_port}/postgres"
        time.sleep(0.2)
    raise RuntimeError("disposable PostgreSQL did not become ready")


async def attach(peer, pane, directory):
    async with connect(peer.pop("worker_websocket"), additional_headers={
        "Authorization": "Bearer " + peer.pop("worker_token"),
    }) as socket:
        (directory / "browser.json").write_text(json.dumps(peer) + "\n")
        print(json.dumps(peer), flush=True)
        await relay(socket, pane)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dist", type=Path)
    parser.add_argument("--test-binary", type=Path, required=True,
                        help="existing it_dashboard binary built from this worktree")
    args = parser.parse_args()
    os.umask(0o077)
    database = "aq-terminal-pilot-" + secrets.token_hex(6)
    shell_container = database + "-shell"
    with tempfile.TemporaryDirectory(prefix="aq-terminal-pilot-") as tmp:
        directory = Path(tmp)
        print(f"Private pilot directory {directory}", flush=True)
        server = pane = None
        try:
            url = start_database(database, secrets.token_hex(24))
            run("docker", "run", "--rm", "-d", "--name", shell_container,
                "--network", "none", "--entrypoint", "sleep",
                "postgres:17-bookworm", "infinity", stdout=subprocess.DEVNULL)
            pane = Pane(directory, shell_container)
            server = subprocess.Popen((
                args.test_binary.resolve(), "terminal_store::pilot::browser_pilot",
                "--exact", "--ignored", "--nocapture",
            ), env={
                **os.environ, "DATABASE_URL": url,
                "AQ_TERMINAL_PILOT_DIR": str(directory),
                "AQ_TERMINAL_PILOT_DASHBOARD_DIR": str(args.dist.resolve(strict=True)),
            })
            asyncio.run(attach(await_peer(server, directory), pane, directory))
        except KeyboardInterrupt:
            pass
        finally:
            (directory / "stop").touch()
            if server is not None:
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    server.terminate()
                    server.wait()
            if pane is not None:
                pane.close()
            subprocess.run(("docker", "stop", "--time", "2", shell_container, database),
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
