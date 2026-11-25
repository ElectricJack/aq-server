#!/usr/bin/env python3
"""End-to-end check of queued work on a live AQ stack, driven only through the server.

Creates one small task in a project, pins it to one machine by draining the others, and
follows it: routed (when routing is on) -> offered -> granted -> finished, then prints the
result's conclusion. On a failure it runs the remote worker check on that machine and prints
the worker's own last log lines, so nothing has to be copied from the machine.

  scripts/e2e-queued-task.py --machine desktop-7lvb545 [--project ID] [--expect-class standard-high]

It leaves the machines running as it found them and cancels its task unless --keep is given.
Exit status: 0 passed, 1 failed, 2 could not run.
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request
import uuid

SCRATCH = "e0000000-0000-4000-8000-000000000002"


class Stack:
    def __init__(self, api):
        self.api = api.rstrip("/")

    def call(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.api + path, data=data, method=method, headers={
            "Accept": "application/json", "Content-Type": "application/json", "X-AQ-Request": "1"})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.status, json.loads(response.read() or b"null")
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                return error.code, json.loads(raw)
            except ValueError:
                return error.code, {"raw": raw[:300].decode("utf-8", "replace")}

    def get(self, path):
        # A read that is briefly unavailable (503) is retried; anything else is the answer.
        for attempt in range(6):
            status, value = self.call("GET", path)
            if status != 503:
                break
            time.sleep(2 + attempt)
        if status != 200:
            raise RuntimeError(f"GET {path}: {status} {json.dumps(value)[:300]}")
        return value

    def action(self, kind, subject, payload, expected=None):
        operation = str(uuid.uuid4())
        status, value = self.call("POST", "/api/v1/actions", {
            "operation_id": operation, "kind": kind, "subject": subject,
            "expected_versions": expected or {}, "payload": {"operation_id": operation, **payload}})
        if status != 200 or value.get("data", {}).get("disposition") != "confirmed":
            raise RuntimeError(f"{kind}: {status} {json.dumps(value)[:400]}")
        return value["data"]

    def machines(self):
        return self.get("/api/v1/swarm")["data"]["machines"]

    def set_machine(self, machine, kind):
        generation = next(m["control"]["generation"] for m in self.machines() if m["id"] == machine)
        self.action(kind, {"type": "machine", "id": machine}, {"machine": machine, "expected_generation": generation})

    def work(self, project, task):
        return self.get(f"/api/human/projects/{project}/tasks/{task}/work")["data"]

    def worker_log(self, machine):
        """The remote check's report for the machine's first node: the worker's own state and log."""
        node = next(m for m in self.machines() if m["id"] == machine)["nodes"][0]["node"]["id"]
        operation = str(uuid.uuid4())
        status, value = self.call("POST", f"/api/worker-diagnostics/{node}", {"operation_id": operation, "kind": "sandbox_startup_v1"})
        if status != 200:
            return [f"remote check not queued: {status} {json.dumps(value)[:200]}"]
        for _ in range(60):
            time.sleep(5)
            row = completed_diagnostic(self.get("/api/worker-diagnostics"), operation)
            if row:
                return diagnostic_lines(row)
        return ["remote check did not answer in 5 minutes"]


def completed_diagnostic(jobs, operation):
    rows = jobs.get("items") or jobs.get("jobs") or jobs.get("data") or []
    row = next((item for item in rows if operation in (item.get("id"), item.get("operation_id"))), None)
    if row and row.get("status") in ("completed", "failed"):
        return row
    return None


def diagnostic_lines(row):
    worker = (row.get("result") or {}).get("details", {}).get("worker", {})
    head = {field: worker.get(field) for field in ("active", "version", "measurement", "missing")}
    return [json.dumps(head), *worker.get("log", [])[-14:]]


def runtime_state(work):
    execution = work.get("execution") or {}
    readiness = work.get("readiness") or {}
    return (execution.get("status"), readiness.get("state")), execution.get("attempt_id"), readiness


def result_for_task(feed, task):
    items = (feed.get("data") or feed).get("items", [])
    return next((item for item in items if item.get("result", {}).get("task_id") == task), None)


def poll(fetch, complete, seconds, interval):
    deadline = time.time() + seconds
    while True:
        value = fetch()
        if complete(value):
            return True, value
        if time.time() > deadline:
            return False, value
        time.sleep(interval)


class AttemptProgress:
    def __init__(self):
        self.state = None
        self.attempts = set()

    def observe(self, work):
        now, attempt, readiness = runtime_state(work)
        if attempt:
            self.attempts.add(attempt)
        if now != self.state:
            reasons = [reason.get("code") for reason in readiness.get("reasons", [])]
            say(f"execution={now[0]} readiness={now[1]} {reasons}")
            self.state = now
        status = now[0]
        if status == "finished":
            return True
        if len(self.attempts) > 1 or status in ("failed", "cancelling", "cancelled"):
            raise RuntimeError(f"the attempt did not finish (attempts so far: {len(self.attempts)}, status {status})")
        return False


def say(text):
    print(f"{time.strftime('%H:%M:%S', time.gmtime())} {text}", flush=True)

def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--api", default="http://127.0.0.1:28080")
    parser.add_argument("--machine", required=True, help="machine id from the Swarm page, e.g. desktop")
    parser.add_argument("--project", default=SCRATCH)
    parser.add_argument("--expect-class", help="with routing on: the level the route must name, e.g. standard-high")
    parser.add_argument("--note", default="e2e", help="a word for the file name and the task title")
    parser.add_argument("--body", help="replace the task's instructions")
    parser.add_argument("--timeout", type=int, default=900, help="seconds to wait for the attempt to finish")
    parser.add_argument("--keep", action="store_true", help="do not cancel the task afterwards")
    return parser.parse_args()


class Probe:
    def __init__(self, stack, args):
        self.stack = stack
        self.args = args

    def create_task(self, name, body):
        revision = self.stack.get(f"/api/human/projects/{self.args.project}/tasks")["project"]["revision"]
        created = self.stack.action("task.create", {"type": "project", "id": self.args.project, "revision": revision}, {
            "expected_project_revision": revision, "title": f"Queued work check ({self.args.note}) on {self.args.machine}",
            "body": body, "acceptance_criteria": f"{name} exists with that one line and nothing else changed.",
            "labels": ["e2e"], "task_state": "active"}, {"project": revision})
        return next(e["id"] for e in created["affected_entities"] if e["type"] == "task")

    def await_route(self, task):
        def routing():
            return self.stack.work(self.args.project, task).get("routing") or {}

        routed, state = poll(routing, self.check_route, 240, 4)
        if not routed:
            raise RuntimeError(f"not routed in 4 minutes: {json.dumps(state)[:200]}")

    def check_route(self, state):
        phase = state.get("state")
        if phase == "unrouted" and state.get("unrouted"):
            raise RuntimeError(f"left unrouted: {json.dumps(state)[:300]}")
        if phase != "routed":
            return False
        route = state["route"]
        say(f"routed: {route.get('class')} harnesses={route.get('harnesses')} prefer={route.get('prefer')}")
        expected = self.args.expect_class
        if expected and route.get("class") != expected:
            raise RuntimeError(f"routed to {route.get('class')}, expected {expected}")
        return True

    def await_attempt(self, task):
        progress = AttemptProgress()
        finished, _ = poll(
            lambda: self.stack.work(self.args.project, task),
            progress.observe, self.args.timeout, 5,
        )
        if not finished:
            raise RuntimeError(f"not finished in {self.args.timeout}s; last {progress.state}")

    def await_result(self, task):
        observed, found = poll(
            lambda: result_for_task(self.stack.get("/api/v1/results"), task),
            bool, 180, 5,
        )
        if not observed:
            raise RuntimeError("the attempt finished but no result reached the server in 3 minutes")
        revision = found.get("revision", {})
        say(f"result: outcome={revision.get('process_outcome')} acceptance={found['assessment'].get('acceptance')}: {str(revision.get('summary'))[:600]}")
        if revision.get("process_outcome") != "success":
            raise RuntimeError("the result's process outcome is not success")

    def diagnose(self):
        try:
            waiting = self.stack.get("/api/v1/swarm")["data"]["waiting"].get("reasons", [])
            for reason in waiting:
                say(f"  swarm: {reason.get('code')}: {reason.get('message')}")
            say(f"  worker on {self.args.machine}:")
            for line in self.stack.worker_log(self.args.machine):
                print("    " + line[:700], flush=True)
        except Exception as inner:  # noqa: BLE001
            say(f"  (no worker log: {inner})")

    def restore(self, task, others):
        try:
            if task and not self.args.keep:
                version = self.stack.work(self.args.project, task)["version"]
                self.stack.action("task.cancel", {"type": "task", "id": task, "revision": version},
                             {"project_id": self.args.project, "expected_version": version}, {"task": version})
        except Exception as error:  # noqa: BLE001
            say(f"cleanup: task not cancelled: {error}")
        for other in others:
            try:
                self.stack.set_machine(other, "worker.start")
            except Exception as error:  # noqa: BLE001
                say(f"cleanup: {other} not restarted: {error}")


def task_definition(args):
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    name = f"notes/{args.note}-{args.machine}-{stamp}.md"
    body = args.body or (f"In the repository, create the file {name} containing exactly one line: "
                         f"Queued work check on {args.machine}, {stamp}. Do not change any other file and do not build anything.")
    return name, body


def main():
    args = parse_args()
    stack = Stack(args.api)
    probe = Probe(stack, args)
    machines = stack.machines()
    if args.machine not in [m["id"] for m in machines]:
        print(f"no machine {args.machine}; have {[m['id'] for m in machines]}", file=sys.stderr)
        return 2
    routing = stack.get("/api/settings/routing")["enabled"]
    others = [m["id"] for m in machines if m["id"] != args.machine and m["control"]["desired"] == "running"]
    name, body = task_definition(args)
    task, step = None, "setup"

    try:
        for other in others:
            stack.set_machine(other, "worker.drain")
        say(f"routing {'on' if routing else 'off'}; target {args.machine}; drained {others or 'nothing'}")
        step = "create"
        task = probe.create_task(name, body)
        say(f"task {task}")

        if routing:
            step = "route"
            probe.await_route(task)

        step = "run"
        probe.await_attempt(task)
        # The worker uploads its result after it stops; cancelling before that refuses the upload.
        step = "result"
        probe.await_result(task)
        say("PASSED: the task was placed, launched and finished with a result on " + args.machine)
    except Exception as error:  # noqa: BLE001 - every failure is reported the same way
        say(f"FAILED at {step}: {error}")
        probe.diagnose()
        return 1
    finally:
        probe.restore(task, others)
    return 0


if __name__ == "__main__":
    sys.exit(main())
