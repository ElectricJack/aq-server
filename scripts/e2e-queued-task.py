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
            jobs = self.get("/api/worker-diagnostics")
            rows = jobs.get("items") or jobs.get("jobs") or jobs.get("data") or []
            row = next((r for r in rows if r.get("id") == operation or r.get("operation_id") == operation), None)
            if row and row.get("status") in ("completed", "failed"):
                worker = (row.get("result") or {}).get("details", {}).get("worker", {})
                head = {k: worker.get(k) for k in ("active", "version", "measurement", "missing")}
                return [json.dumps(head)] + [line for line in worker.get("log", [])[-14:]]
        return ["remote check did not answer in 5 minutes"]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--api", default="http://127.0.0.1:28080")
    parser.add_argument("--machine", required=True, help="machine id from the Swarm page, e.g. desktop")
    parser.add_argument("--project", default=SCRATCH)
    parser.add_argument("--expect-class", help="with routing on: the level the route must name, e.g. standard-high")
    parser.add_argument("--note", default="e2e", help="a word for the file name and the task title")
    parser.add_argument("--body", help="replace the task's instructions")
    parser.add_argument("--timeout", type=int, default=900, help="seconds to wait for the attempt to finish")
    parser.add_argument("--keep", action="store_true", help="do not cancel the task afterwards")
    args = parser.parse_args()
    stack = Stack(args.api)

    machines = stack.machines()
    if args.machine not in [m["id"] for m in machines]:
        print(f"no machine {args.machine}; have {[m['id'] for m in machines]}", file=sys.stderr)
        return 2
    routing = stack.get("/api/settings/routing")["enabled"]
    others = [m["id"] for m in machines if m["id"] != args.machine and (m.get("control") or {}).get("desired") == "running"]
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    name = f"notes/{args.note}-{args.machine}-{stamp}.md"
    body = args.body or (f"In the repository, create the file {name} containing exactly one line: "
                         f"Queued work check on {args.machine}, {stamp}. Do not change any other file and do not build anything.")
    task, failed, step = None, None, "setup"

    def say(text):
        print(f"{time.strftime('%H:%M:%S', time.gmtime())} {text}", flush=True)

    try:
        for other in others:
            stack.set_machine(other, "worker.drain")
        say(f"routing {'on' if routing else 'off'}; target {args.machine}; drained {others or 'nothing'}")
        step = "create"
        revision = stack.get(f"/api/human/projects/{args.project}/tasks")["project"]["revision"]
        created = stack.action("task.create", {"type": "project", "id": args.project, "revision": revision}, {
            "expected_project_revision": revision, "title": f"Queued work check ({args.note}) on {args.machine}",
            "body": body, "acceptance_criteria": f"{name} exists with that one line and nothing else changed.",
            "labels": ["e2e"], "task_state": "active"}, {"project": revision})
        task = next(e["id"] for e in created["affected_entities"] if e["type"] == "task")
        say(f"task {task}")

        if routing:
            step = "route"
            deadline = time.time() + 240
            while True:
                state = stack.work(args.project, task).get("routing") or {}
                if state.get("state") == "routed":
                    route = state["route"]
                    say(f"routed: {route.get('class')} harnesses={route.get('harnesses')} prefer={route.get('prefer')}")
                    if args.expect_class and route.get("class") != args.expect_class:
                        raise RuntimeError(f"routed to {route.get('class')}, expected {args.expect_class}")
                    break
                if state.get("state") == "unrouted" and state.get("unrouted"):
                    raise RuntimeError(f"left unrouted: {json.dumps(state)[:300]}")
                if time.time() > deadline:
                    raise RuntimeError(f"not routed in 4 minutes: {json.dumps(state)[:200]}")
                time.sleep(4)

        step = "run"
        seen, deadline, attempts = None, time.time() + args.timeout, set()
        while True:
            work = stack.work(args.project, task)
            execution, readiness = work.get("execution") or {}, work.get("readiness") or {}
            now = (execution.get("status"), readiness.get("state"))
            # An offer that lapses before it is granted never ran anything.
            if execution.get("attempt_id") and now[0] != "offered":
                attempts.add(execution["attempt_id"])
            if now != seen:
                say(f"execution={now[0]} readiness={now[1]} {[r.get('code') for r in readiness.get('reasons', [])]}")
                seen = now
            if now[0] == "finished":
                break
            if len(attempts) > 1 or now[0] in ("failed", "cancelling", "cancelled"):
                raise RuntimeError(f"the attempt did not finish (attempts so far: {len(attempts)}, status {now[0]})")
            if time.time() > deadline:
                raise RuntimeError(f"not finished in {args.timeout}s; last {now}")
            time.sleep(5)
        # The worker uploads its result after it stops; cancelling before that refuses the upload.
        step = "result"
        deadline = time.time() + 180
        while True:
            feed = stack.get("/api/v1/results")
            items = (feed.get("data") or feed).get("items", [])
            found = next((i for i in items if i.get("result", {}).get("task_id") == task), None)
            if found:
                revision = found.get("revision", {})
                say(f"result: outcome={revision.get('process_outcome')} acceptance={found['assessment'].get('acceptance')}: {str(revision.get('summary'))[:600]}")
                if revision.get("process_outcome") != "success":
                    raise RuntimeError("the result's process outcome is not success")
                break
            if time.time() > deadline:
                raise RuntimeError("the attempt finished but no result reached the server in 3 minutes")
            time.sleep(5)
        say("PASSED: the task was placed, launched and finished with a result on " + args.machine)
    except Exception as error:  # noqa: BLE001 - every failure is reported the same way
        failed = f"{step}: {error}"
        say("FAILED at " + failed)
        try:
            waiting = stack.get("/api/v1/swarm")["data"]["waiting"].get("reasons", [])
            for reason in waiting:
                say(f"  swarm: {reason.get('code')}: {reason.get('message')}")
            say(f"  worker on {args.machine}:")
            for line in stack.worker_log(args.machine):
                print("    " + line[:700], flush=True)
        except Exception as inner:  # noqa: BLE001
            say(f"  (no worker log: {inner})")
    finally:
        try:
            if task and not args.keep:
                version = stack.work(args.project, task)["version"]
                stack.action("task.cancel", {"type": "task", "id": task, "revision": version},
                             {"project_id": args.project, "expected_version": version}, {"task": version})
        except Exception as error:  # noqa: BLE001
            say(f"cleanup: task not cancelled: {error}")
        for other in others:
            try:
                stack.set_machine(other, "worker.start")
            except Exception as error:  # noqa: BLE001
                say(f"cleanup: {other} not restarted: {error}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
