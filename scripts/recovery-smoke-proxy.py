#!/usr/bin/env python3
"""Fixture-only HTTP fault proxy. Never logs credentials or request payloads."""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import os
import socket
import threading
import time
from typing import NamedTuple
import urllib.error
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--control", type=Path, required=True)
parser.add_argument("--upstream", default="http://server:8081")
parser.add_argument("--port", type=int, default=8081)
args = parser.parse_args()
lock = threading.Lock()
seen = set()


def event(value):
    value["observed_monotonic_ns"] = time.monotonic_ns()
    with lock:
        with (args.control / "events.jsonl").open("a") as output:
            output.write(json.dumps(value, sort_keys=True) + "\n")
            output.flush()
            os.fsync(output.fileno())


class Fault(NamedTuple):
    mode: str
    token: str
    barrier_before_drop: bool


def valid_token(token):
    return token.replace("-", "").isalnum() and len(token) <= 100


def current_fault(rule, token):
    with lock:
        if rule.get("once"):
            if token in seen:
                return Fault("pass", token, False)
            seen.add(token)
    return Fault(rule.get("mode", "pass"), token, rule.get("barrier_before_drop"))


def wait_at_barrier(token, observation, stage):
    marker = args.control / (token + ".ready")
    marker.write_text(json.dumps({**observation, "stage": stage}))
    event({**observation, "event": stage})
    deadline = time.monotonic() + 90
    while not (args.control / (token + ".release")).exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("fixture barrier not released")
        time.sleep(0.01)


def grant_observation(path, status, data):
    if path not in {"/v2/worker/accept", "/v2/worker/renew"} or not 200 <= status < 300:
        return {}
    grant = json.loads(data)["grant"]
    immutable = {key: value for key, value in grant.items() if key != "valid_for_ms"}
    canonical = json.dumps(immutable, sort_keys=True, separators=(",", ":")).encode()
    return {"immutable_grant_sha256": hashlib.sha256(canonical).hexdigest(),
            "grant_remaining_ms": grant["valid_for_ms"]}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *unused):
        pass

    def reply(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        if size > 1024 * 1024 or size < 0:
            return self.reply(413, b'{}')
        body = self.rfile.read(size)
        value = json.loads(body) if body else {}
        identity = value.get("operation_id", value.get("report_id"))
        observation = {"path": self.path, "operation_id": identity,
                       "body_sha256": hashlib.sha256(body).hexdigest()}
        rules = json.loads((args.control / "rules.json").read_text())
        rule = rules.get(self.path, {})
        token = rule.get("token", "")
        if token and not valid_token(token):
            return self.reply(500, b'{}')
        fault = current_fault(rule, token)
        observation["fault"] = fault.mode
        try:
            self.proxy_request(body, observation, fault)
        except (OSError, TimeoutError) as error:
            event({**observation, "event": "proxy_transport_error", "error_type": type(error).__name__})
            self.close_connection = True

    def proxy_request(self, body, observation, fault):
        if fault.mode == "partition":
            event({**observation, "event": "partition_refused_before_forward"})
            return self.reply(503, b'{"code":"overloaded","message":"fixture partition","retryable":true,"operation_id":null}')
        if fault.mode == "hold_before_forward":
            wait_at_barrier(fault.token, observation, "held_before_forward")
        status, data = self.forward_request(body, observation)
        if 200 <= status < 300 and self.handle_committed_response(observation, fault):
            return
        self.reply(status, data)

    def forward_request(self, body, observation):
        request = urllib.request.Request(args.upstream + self.path, body, {
            "Content-Type": "application/json", "Authorization": self.headers.get("Authorization", "")})
        started = time.monotonic_ns()
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                status, data = response.status, response.read()
        except urllib.error.HTTPError as response:
            status, data = response.code, response.read()
        grant_fields = grant_observation(self.path, status, data)
        event({**observation, **grant_fields, "event": "upstream_response", "status": status,
               "upstream_latency_ms": (time.monotonic_ns() - started) / 1_000_000,
               "response_sha256": hashlib.sha256(data).hexdigest()})
        return status, data

    def handle_committed_response(self, observation, fault):
        if fault.mode == "hold_after_commit":
            wait_at_barrier(fault.token, observation, "held_after_upstream_commit")
        if fault.mode != "drop_after_commit":
            return False
        if fault.barrier_before_drop:
            wait_at_barrier(fault.token, observation, "committed_before_connection_drop")
        event({**observation, "event": "connection_closed_after_upstream_commit"})
        self.close_connection = True
        self.connection.shutdown(socket.SHUT_RDWR)
        self.connection.close()
        return True


server = ThreadingHTTPServer(("0.0.0.0", args.port), Handler)
server.daemon_threads = True
event({"event": "proxy_ready", "port": server.server_port})
server.serve_forever(poll_interval=0.1)
