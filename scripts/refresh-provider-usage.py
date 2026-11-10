#!/usr/bin/env python3
"""Fetch subscription quota through installed clients. Never sends a model prompt.

Publish only bounded percentages/reset timestamps; failed sources retain their
original observation time. No API keys, OAuth parsing or private HTTP endpoints.
"""
import argparse
import concurrent.futures
import datetime
import importlib.util
import json
import math
from pathlib import Path
import subprocess
import time

spec = importlib.util.spec_from_file_location("catalog", Path(__file__).with_name("refresh-model-catalog.py"))
catalog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(catalog)


def window(identity, label, percent, reset):
    if isinstance(percent, bool) or not isinstance(percent, (int, float)) or not math.isfinite(percent) or not 0 <= percent <= 100:
        raise ValueError("unknown percentage")
    if isinstance(reset, str):
        reset = int(datetime.datetime.fromisoformat(reset.replace("Z", "+00:00")).timestamp())
    if reset is not None and (isinstance(reset, bool) or not isinstance(reset, int) or reset <= 0):
        raise ValueError("invalid reset")
    return dict(id=identity, label=label, used_percent=percent, reset_at=reset)


def codex(cwd):
    client = catalog.Rpc(["codex", "app-server", "--stdio"], cwd)
    try:
        client.request("initialize", {"clientInfo": {"name": "aq_provider_usage", "version": "1"}, "capabilities": {}})
        client.send("initialized", {})
        value = client.request("account/rateLimits/read", {})
        rate = (value.get("rateLimitsByLimitId") or {}).get("codex") or value["rateLimits"]
        windows = []
        for key in ("primary", "secondary"):
            row = rate.get(key)
            if row:
                minutes = row.get("windowDurationMins")
                label = {300: "5 hours", 10080: "Weekly", 1440: "Daily"}.get(minutes, f"{minutes} minutes" if isinstance(minutes, int) and 0 < minutes < 100000 else key.title())
                windows.append(window(key, label, row["usedPercent"], row.get("resetsAt")))
        return windows
    finally:
        client.close()


def claude(cwd, sdk, executable):
    result = subprocess.run(["node", str(Path(__file__).with_name("provider-usage-claude.mjs")), sdk, executable], cwd=cwd,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=35, check=True)
    if len(result.stdout) > 65536:
        raise ValueError("usage response too large")
    value = json.loads(result.stdout)
    if value.get("rate_limits_available") is not True or not isinstance(value.get("rate_limits"), dict):
        raise ValueError("subscription usage unavailable")
    limits, windows = value["rate_limits"], []
    for key, label in [("five_hour", "5 hours"), ("seven_day", "Weekly"), ("seven_day_opus", "Weekly Opus"), ("seven_day_sonnet", "Weekly Sonnet"), ("seven_day_oauth_apps", "Weekly OAuth apps")]:
        row = limits.get(key)
        if isinstance(row, dict) and row.get("utilization") is not None:
            windows.append(window(key, label, row["utilization"], row.get("resets_at")))
    return windows


def refresh(output, cwd, sdk, executable, claude_account, codex_account):
    try:
        old = json.loads(output.read_text())
        retained = {(a["provider"], a["account_ref"]): a for a in old["accounts"]}
    except (OSError, ValueError, KeyError, TypeError):
        retained = {}
    accounts, statuses = [], {}
    sources = [("anthropic", claude_account, lambda: claude(cwd, sdk, executable)), ("openai", codex_account, lambda: codex(cwd))]
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        jobs = [(p, a, pool.submit(fetch)) for p, a, fetch in sources]
        for provider, account, job in jobs:
            try:
                windows = job.result()
                if not windows or len(windows) > 16:
                    raise ValueError("no windows")
                accounts.append(dict(provider=provider, account_ref=account, observed_at_ms=int(time.time()*1000), windows=windows))
                statuses[provider] = "refreshed"
            except Exception:
                previous = retained.get((provider, account))
                if previous:
                    accounts.append(previous)
                statuses[provider] = "retained stale reading" if previous else "unavailable"
    catalog.atomic_json(output, dict(schema_version=1, accounts=accounts), mode=0o644)
    return statuses


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cwd", default="/tmp")
    parser.add_argument("--claude-sdk", required=True)
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--claude-account", default="owner-claude-subscription")
    parser.add_argument("--codex-account", default="Codex ChatGPT")
    args = parser.parse_args()
    print(json.dumps(refresh(args.output, args.cwd, args.claude_sdk, args.claude, args.claude_account, args.codex_account)))


if __name__ == "__main__":
    main()
