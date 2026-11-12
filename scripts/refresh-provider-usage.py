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


def reset_unknown():
    return dict(state="unknown", available_count=None, grants=None)


def codex_resets(value):
    """Normalize documented app-server reset credits without copying opaque IDs."""
    block = value.get("rateLimitResetCredits")
    if not isinstance(block, dict):
        return reset_unknown()
    count = block.get("availableCount")
    if isinstance(count, bool) or not isinstance(count, int) or not 0 <= count <= 1_000_000:
        return reset_unknown()
    details = block.get("credits")
    grants = None
    if isinstance(details, list) and len(details) <= 100:
        grants = []
        for row in details:
            if not isinstance(row, dict) or row.get("status") != "available":
                continue
            label = row.get("title")
            if label is None:
                label = "Rate-limit reset"
            if not isinstance(label, str) or not label.strip() or len(label) > 120 or any(ord(c) < 32 for c in label):
                continue
            expiry = row.get("expiresAt")
            if expiry is not None:
                if isinstance(expiry, bool) or not isinstance(expiry, int) or not 0 < expiry <= 253402300799:
                    continue
                try:
                    expiry = datetime.datetime.fromtimestamp(expiry, datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
                except (ValueError, OSError, OverflowError):
                    continue
            # Availability of a credit does not establish that an eligible window
            # can be reset now; the read interface supplies no such assertion.
            grants.append(dict(label=label, remaining_count=None, expires_at=expiry, usable_now=None, clears=[]))
    return dict(state="known", available_count=count, grants=grants)


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
        return windows, codex_resets(value)
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
    # The documented experimental SDK control exposes windows, not grants.
    return claude_windows(value["rate_limits"]), reset_unknown()


def claude_windows(limits):
    windows = []
    for key, label in [("five_hour", "5 hours"), ("seven_day", "Weekly"), ("seven_day_opus", "Weekly Opus"), ("seven_day_sonnet", "Weekly Sonnet"), ("seven_day_oauth_apps", "Weekly OAuth apps")]:
        row = limits.get(key)
        if isinstance(row, dict) and row.get("utilization") is not None:
            windows.append(window(key, label, row["utilization"], row.get("resets_at")))
    return windows


def cached_claude(path, expected_account):
    """Read CLI-produced quota only, bound to an explicit operator account ID.

    Preserve the provider observation time: reading an old cache is never a
    fresh quota request. No credentials, session costs or token sums are read.
    """
    if path is None or not expected_account or path.is_symlink():
        return None
    meta = path.stat()
    if not path.is_file() or meta.st_mode & 0o022 or meta.st_size > 4_194_304:
        return None
    value = json.loads(path.read_text()).get("cachedUsageUtilization", {})
    observed = value.get("fetchedAtMs")
    if value.get("accountUuid") != expected_account or isinstance(observed, bool) or not isinstance(observed, int) or not 0 < observed <= int(time.time()*1000)+5000:
        return None
    windows = claude_windows(value.get("utilization", {}))
    return (observed, windows) if windows else None


def refresh(output, cwd, sdk, executable, claude_account, codex_account, claude_cache=None, claude_account_id=None):
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
                windows, resets = job.result()
                if not windows or len(windows) > 16:
                    raise ValueError("no windows")
                observed = int(time.time()*1000)
                resets["observed_at_ms"] = observed
                accounts.append(dict(provider=provider, account_ref=account, observed_at_ms=observed, windows=windows, reset_credits=resets))
                statuses[provider] = "refreshed"
            except Exception:
                previous = retained.get((provider, account))
                if provider == "anthropic":
                    try:
                        cached = cached_claude(claude_cache, claude_account_id)
                        if cached and (previous is None or cached[0] > previous["observed_at_ms"]):
                            reset = previous.get("reset_credits") if previous else None
                            previous = dict(provider=provider, account_ref=account, observed_at_ms=cached[0], windows=cached[1], source="claude_cli_usage_cache")
                            if reset is not None:
                                previous["reset_credits"] = reset
                    except (OSError, ValueError, KeyError, TypeError):
                        pass
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
    parser.add_argument("--claude-cache", type=Path, help="Optional Claude CLI quota cache; observation timestamps are retained")
    parser.add_argument("--claude-account-id", help="Explicit account identity required for the optional quota cache")
    args = parser.parse_args()
    print(json.dumps(refresh(args.output, args.cwd, args.claude_sdk, args.claude, args.claude_account, args.codex_account, args.claude_cache, args.claude_account_id)))


if __name__ == "__main__":
    main()
