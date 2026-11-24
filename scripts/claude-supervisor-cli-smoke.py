#!/usr/bin/env python3
"""Qualify the pinned Claude supervisor invocation with loopback HTTP doubles.

Pass --host-binary to exercise the Rust MCP bridge and its authenticated AQ
discovery and task creation requests. Otherwise a Python fixture speaks stdio MCP.
"""

import argparse
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

VERSION = "2.1.318"
CLAUDE_TOKEN = "dummy-claude-oauth-token"
AQ_TOKEN = "dummy-aq-supervisor-token"
AQ_RESULT = "qualification-discovery-marker"
OPERATION_ID = "70ae8248-4b12-41a4-9b65-abc4335e06e3"
CREATE_REQUEST = {
    "operation_id": OPERATION_ID,
    "params": {"title": "qualification task"},
}
CREATE_REPLY = {
    "data": {"disposition": "confirmed", "title": "qualification-created-marker"}
}
PROMPT = "qualification-user-marker"
SYSTEM = "qualification-system-marker"
RESULT = "qualification-result-marker"
TOOLS = ["aq_discover", "aq_describe", "aq_command", "aq_attachment"]


def mcp():
    assert os.environ["AQ_SUPERVISOR_ORIGIN"].startswith("http://127.0.0.1:")
    assert "CLAUDE_CODE_OAUTH_TOKEN" not in os.environ
    for line in sys.stdin:
        request = json.loads(line)
        method = request["method"]
        if "id" not in request:
            continue
        if method == "initialize":
            result = {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "aq-qualification", "version": "1"},
            }
        elif method == "tools/list":
            result = {
                "tools": [
                    {
                        "name": name,
                        "description": name,
                        "inputSchema": {"type": "object"},
                    }
                    for name in TOOLS
                ],
            }
        elif method == "tools/call":
            assert request["params"]["name"] == "aq_discover"
            result = {"content": [{"type": "text", "text": AQ_RESULT}]}
        else:
            raise ValueError(method)
        print(
            json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}),
            flush=True,
        )


class MockAPI(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path != "/api/v1/commands":
            self.send_error(404)
            return
        if self.headers.get("Authorization") != f"Bearer {AQ_TOKEN}":
            self.send_error(401)
            return
        self.server.discovery_calls.append(self.path)
        self.send_json(
            {
                "api_version": 1,
                "principal": AQ_RESULT,
                "commands": [],
                "unavailable": [],
            }
        )

    def send_json(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_POST(self):
        if self.path == "/api/v1/commands/task.create":
            if self.headers.get("Authorization") != f"Bearer {AQ_TOKEN}":
                self.send_error(401)
                return
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            self.server.create_calls.append(body)
            if any(body.get(key) != value for key, value in CREATE_REQUEST.items()):
                self.send_error(400)
                return
            self.send_json(CREATE_REPLY)
            return
        if not self.path.startswith("/v1/messages?") and self.path != "/v1/messages":
            self.send_error(404)
            return

        if self.headers.get("Authorization") != f"Bearer {CLAUDE_TOKEN}":
            self.send_error(401)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.calls.append(body)
        turn = len(self.server.calls)
        if turn == 1:
            tool_name, tool_input = "aq_discover", {}
        elif turn == 2 and self.server.exercise_mutation:
            tool_name = "aq_command"
            tool_input = {"name": "task.create", "request": CREATE_REQUEST}
        else:
            tool_name = None

        if tool_name:
            block = {
                "type": "tool_use",
                "id": f"toolu_qualification_{turn}",
                "name": f"mcp__aq__{tool_name}",
                "input": tool_input,
            }
            stop_reason = "tool_use"
        else:
            block = {"type": "text", "text": RESULT}
            stop_reason = "end_turn"
        usage = {"input_tokens": 10, "output_tokens": 1}
        message = {
            "id": f"msg_qualification_{len(self.server.calls)}",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": body["model"],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": usage,
        }
        if not body.get("stream"):
            message.update(content=[block], stop_reason=stop_reason)
            self.send_json(message)
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        if block["type"] == "text":
            delta = {"type": "text_delta", "text": block["text"]}
            block["text"] = ""
        else:
            delta = {
                "type": "input_json_delta",
                "partial_json": json.dumps(block["input"]),
            }
            block["input"] = {}
        events = [
            {"type": "message_start", "message": message},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                "usage": {"output_tokens": 10},
            },
            {"type": "message_stop"},
        ]
        for event in events:
            self.wfile.write(
                f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode()
            )


def qualify(cli, host_binary):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory).resolve()
        home = root / "home"
        cwd = root / "cwd"
        home.mkdir()
        cwd.mkdir()
        credentials = root / "credentials"
        credentials.touch(mode=0o600)
        credentials.write_text(AQ_TOKEN)
        actions = root / "actions"
        with http.server.HTTPServer(("127.0.0.1", 0), MockAPI) as server:
            server.calls = []
            server.discovery_calls = []
            server.create_calls = []
            server.exercise_mutation = bool(host_binary)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                endpoint = f"http://127.0.0.1:{server.server_port}"
                env = {
                    "HOME": str(home),
                    "PATH": "/usr/local/bin:/usr/bin:/bin",
                    "CLAUDE_CODE_OAUTH_TOKEN": CLAUDE_TOKEN,
                    "ANTHROPIC_BASE_URL": endpoint,
                    "CLAUDE_CONFIG_DIR": str(home),
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                }
                version = subprocess.run(
                    [cli, "--version"],
                    cwd=cwd,
                    env=env,
                    capture_output=True,
                    text=True,
                    check=True,
                    timeout=10,
                ).stdout.strip()
                assert version == f"{VERSION} (Claude Code)", (
                    f"Unexpected CLI version: {version}"
                )
                if host_binary:
                    mcp_command = [host_binary, "mcp"]
                else:
                    mcp_command = [
                        sys.executable,
                        str(Path(__file__).resolve()),
                        "--mcp",
                    ]
                config = {
                    "mcpServers": {
                        "aq": {
                            "command": "/usr/bin/env",
                            "args": [
                                "-i",
                                f"HOME={home}",
                                f"PATH={env['PATH']}",
                                f"AQ_SUPERVISOR_ORIGIN={endpoint}",
                                f"AQ_SUPERVISOR_CREDENTIAL_FILE={credentials}",
                                f"AQ_SUPERVISOR_ACTIONS_FILE={actions}",
                                *mcp_command,
                            ],
                        }
                    }
                }
                argv = [
                    cli,
                    "-p",
                    "--output-format",
                    "json",
                    "--input-format",
                    "text",
                    "--model",
                    "claude-sonnet-4-6",
                    "--setting-sources",
                    "",
                    "--settings",
                    '{"disableAllHooks":true,"enableAllProjectMcpServers":false}',
                    "--strict-mcp-config",
                    "--restricted",
                    "--no-session-persistence",
                    "--disable-slash-commands",
                    "--tools",
                    "",
                    "--permission-mode",
                    "dontAsk",
                    "--append-system-prompt",
                    SYSTEM,
                    "--mcp-config",
                    json.dumps(config),
                    "--allowedTools",
                    *(f"mcp__aq__{name}" for name in TOOLS),
                ]
                reply = subprocess.run(
                    argv,
                    cwd=cwd,
                    env=env,
                    text=True,
                    input=PROMPT,
                    capture_output=True,
                    timeout=25,
                )
                assert reply.returncode == 0, (
                    f"Claude exited {reply.returncode} with {len(server.calls)} model requests"
                    f"\nstderr: {reply.stderr}\nstdout: {reply.stdout}"
                )
                result = json.loads(reply.stdout)
                assert result["type"] == "result" and result["subtype"] == "success"
                assert not result["is_error"] and result["result"] == RESULT
                assert len(server.calls) == 2 + bool(host_binary)
                first, second, *final = server.calls
                assert SYSTEM in json.dumps(first["system"])
                assert PROMPT in json.dumps(first["messages"])
                advertised = {tool["name"] for tool in first["tools"]}
                required = {f"mcp__aq__{name}" for name in TOOLS}
                allowed = required | {"mcp__aq__repo_sync", "mcp__aq__repo_read"}
                assert required <= advertised <= allowed, (
                    f"Unexpected tools: {sorted(advertised)}"
                )
                assert AQ_RESULT in json.dumps(second["messages"])
                if host_binary:
                    assert len(server.discovery_calls) == len(server.create_calls) == 1
                    assert CREATE_REPLY["data"]["title"] in json.dumps(
                        final[0]["messages"]
                    )
                    assert actions.read_text() == f"{OPERATION_ID}\n"
                    print(
                        f"{version}: MCP discovery, task receipt, and JSON result passed"
                    )
                else:
                    print(f"{version}: MCP discovery and JSON result passed")
            finally:
                server.shutdown()
                thread.join()


if __name__ == "__main__":
    if sys.argv[1:] == ["--mcp"]:
        mcp()
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument(
            "claude_binary", type=lambda value: str(Path(value).resolve(strict=True))
        )
        parser.add_argument(
            "--host-binary", type=lambda value: str(Path(value).resolve(strict=True))
        )
        args = parser.parse_args()
        qualify(args.claude_binary, args.host_binary)
