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
from urllib.error import URLError
from urllib.request import Request, urlopen
from uuid import UUID, uuid4

from websockets.asyncio.client import connect

HOME_PROJECT = "a22afccc-d61a-5f77-aef8-9fd73311c68a"
RETAINED_TASK = "a3c72f06-d1a6-428e-bf67-b0f1c734a8c9"
RETAINED_TITLE = "Fix black terminal when connecting to running task agents"
GRAPH_IMAGE = "sha256:da747555d632db4b5d90e577e356443403f9a9c1f1d2bf0bf1e0fa2017f4c70c"


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


def mapped_port(name, port):
    return run("docker", "port", name, f"{port}/tcp",
               capture_output=True, text=True).stdout.strip().rsplit(":", 1)[1]


def start_database(name, password, network):
    run("docker", "run", "--pull", "never", "--rm", "-d", "--name", name,
        "--network", network,
        "-e", "POSTGRES_PASSWORD", "-p", "127.0.0.1::5432",
        "postgres:17-bookworm", "-c", "max_locks_per_transaction=512",
        stdout=subprocess.DEVNULL, env={**os.environ, "POSTGRES_PASSWORD": password})
    host_port = mapped_port(name, 5432)
    for _ in range(100):
        ready = subprocess.run(
            ("docker", "exec", name, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
        )
        if ready.returncode == 0:
            return f"postgresql://postgres:{password}@127.0.0.1:{host_port}/postgres"
        time.sleep(0.2)
    raise RuntimeError("disposable PostgreSQL did not become ready")


def seed_graph(database, password, network, directory, image):
    run("docker", "exec", "--user", "postgres", database, "createdb", "pilot_graph")
    token = secrets.token_hex(32)
    token_file = directory / "graph.token"
    token_file.write_text(token + "\n")
    workspace = str(uuid4())
    graph = database + "-graph"
    run("docker", "run", "--pull", "never", "--rm", "-d", "--name", graph,
        "--network", network, "-p", "127.0.0.1::8741",
        "-e", "DATABASE_URL",
        "-e", "AQ_GRAPH_BIND=0.0.0.0:8741",
        "-e", "AQ_GRAPH_POOL_SIZE=4",
        "-e", f"AQ_GRAPH_BOOTSTRAP_WORKSPACE={workspace}",
        "-e", "AQ_GRAPH_BOOTSTRAP_ACTOR=pilot-admin",
        "-e", "AQ_GRAPH_BOOTSTRAP_TOKEN",
        image, stdout=subprocess.DEVNULL, env={
            **os.environ,
            "DATABASE_URL": f"postgresql://postgres:{password}@{database}/pilot_graph",
            "AQ_GRAPH_BOOTSTRAP_TOKEN": token,
        })
    origin = "http://127.0.0.1:" + mapped_port(graph, 8741)
    for _ in range(120):
        try:
            with urlopen(origin + "/health/ready", timeout=1):
                break
        except (URLError, TimeoutError, ConnectionError):
            time.sleep(0.25)
    else:
        raise RuntimeError("disposable Graph did not become ready")
    request = Request(
        f"{origin}/v1/workspaces/{workspace}/mutations",
        data=json.dumps({
            "idempotency_key": "terminal-pilot-original-task",
            "operations": [{
                "op": "create_node", "kind": "task", "id": RETAINED_TASK,
                "title": RETAINED_TITLE, "task_state": "active",
                "metadata": {"aq:project_id": HOME_PROJECT},
            }],
        }).encode(),
        headers={"Authorization": "Bearer " + token,
                 "Content-Type": "application/json", "X-AQ-Graph-API-Version": "1"},
    )
    with urlopen(request, timeout=5) as response:
        assert response.status == 200
    return {
        "AQ_TERMINAL_PILOT_GRAPH_ORIGIN": origin,
        "AQ_TERMINAL_PILOT_GRAPH_TOKEN": str(token_file),
        "AQ_TERMINAL_PILOT_WORKSPACE": workspace,
        "AQ_TERMINAL_PILOT_HOME": HOME_PROJECT,
        "AQ_TERMINAL_PILOT_TASK": RETAINED_TASK,
    }


async def attach(peer, pane, directory, workspace_only):
    async with connect(peer.pop("worker_websocket"), additional_headers={
        "Authorization": "Bearer " + peer.pop("worker_token"),
    }) as socket:
        (directory / "browser.json").write_text(json.dumps(peer) + "\n")
        print(json.dumps(peer), flush=True)
        if not workspace_only:
            await relay(socket, pane)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dist", type=Path)
    parser.add_argument("--test-binary", type=Path, required=True,
                        help="existing it_dashboard binary built from this worktree")
    parser.add_argument("--graph-image", default=GRAPH_IMAGE)
    parser.add_argument("--workspace-git", type=Path,
                        help="repositories.json from the private workspace Git stage")
    parser.add_argument("--workspace-only", action="store_true",
                        help="exit after the authenticated workspace assertions")
    args = parser.parse_args()
    if args.workspace_only and args.workspace_git is None:
        parser.error("--workspace-only requires --workspace-git")
    os.umask(0o077)
    database = "aq-terminal-pilot-" + secrets.token_hex(6)
    shell_container = database + "-shell"
    with tempfile.TemporaryDirectory(prefix="aq-terminal-pilot-") as tmp:
        directory = Path(tmp)
        print(f"Private pilot directory {directory}", flush=True)
        server = pane = None
        try:
            run("docker", "network", "create", database,
                stdout=subprocess.DEVNULL)
            database_password = secrets.token_hex(24)
            url = start_database(database, database_password, database)
            graph = seed_graph(database, database_password, database, directory, args.graph_image)
            run("docker", "run", "--pull", "never", "--rm", "-d", "--name", shell_container,
                "--network", "none", "--entrypoint", "sleep",
                "postgres:17-bookworm", "infinity", stdout=subprocess.DEVNULL)
            pane = Pane(directory, shell_container)
            workspace = {}
            if args.workspace_git is not None:
                workspace["AQ_TERMINAL_PILOT_GIT_INVENTORY"] = str(args.workspace_git.resolve(strict=True))
            server = subprocess.Popen((
                args.test_binary.resolve(), "terminal_store::pilot::browser_pilot",
                "--exact", "--ignored", "--nocapture",
            ), env={
                **os.environ, **graph, **workspace, "DATABASE_URL": url,
                "AQ_TERMINAL_PILOT_DIR": str(directory),
                "AQ_TERMINAL_PILOT_DASHBOARD_DIR": str(args.dist.resolve(strict=True)),
            })
            asyncio.run(attach(await_peer(server, directory), pane, directory, args.workspace_only))
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
            subprocess.run(
                ("docker", "stop", "--time", "2", shell_container, database + "-graph", database),
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
            )
            subprocess.run(("docker", "network", "rm", database),
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
