#!/usr/bin/env python3
"""Refresh picker metadata without prompts, inference, API keys, or configuration edits.

Official subscription clients supply account model lists. The public OpenCode
catalog supplies remaining model/effort metadata, explicitly without access claims.
Failed sources retain their last snapshot; output replacement is atomic.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time
import urllib.request


def fetch(url, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Accept": "application/json", "Content-Type": "application/json", "User-Agent": "aq-model-catalog/1"})
    with urllib.request.urlopen(req, timeout=20) as response:
        raw = response.read(12 * 1024 * 1024 + 1)
    if len(raw) > 12 * 1024 * 1024:
        raise ValueError("metadata response too large")
    return json.loads(raw)


class Rpc:
    def __init__(self, command, cwd):
        env = {k: v for k, v in os.environ.items() if k not in {"OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"}}
        self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.identity = 0

    def send(self, method, params, identity=None):
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if identity is not None:
            message["id"] = identity
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.identity += 1
        self.send(method, params, self.identity)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if b"\n" not in self.buffer:
                if not self.selector.select(1):
                    continue
                chunk = os.read(self.process.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("metadata client exited")
                self.buffer += chunk
                if len(self.buffer) > 2 * 1024 * 1024:
                    raise ValueError("metadata client response too large")
            if b"\n" not in self.buffer:
                continue
            line, self.buffer = self.buffer.split(b"\n", 1)
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if value.get("id") == self.identity:
                if "error" in value:
                    raise RuntimeError("metadata client refused discovery")
                return value["result"]
        raise TimeoutError("metadata client timed out")

    def close(self):
        self.selector.close()
        self.process.terminate()
        try:
            self.process.wait(5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def codex_models(cwd):
    client = Rpc(["codex", "app-server", "--stdio"], cwd)
    try:
        client.request("initialize", {"clientInfo": {"name": "aq_model_catalog", "version": "1"}, "capabilities": {}})
        client.send("initialized", {})
        models, cursor = [], None
        for _ in range(32):
            result = client.request("model/list", {"limit": 100, "includeHidden": True, "cursor": cursor})
            models.extend(result["data"])
            cursor = result.get("nextCursor")
            if not cursor:
                return models
        raise ValueError("too many model pages")
    finally:
        client.close()


def gemini_models(cwd):
    client = Rpc(["gemini", "--acp"], cwd)
    try:
        client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "aq-model-catalog", "version": "1"}})
        return client.request("session/new", {"cwd": cwd, "mcpServers": []})["models"]["availableModels"]
    finally:
        client.close()


def choice(provider, model, effort, options, harness, source, observed):
    identity = json.dumps([provider, model, options, harness], sort_keys=True, separators=(",", ":")).encode()
    return dict(provider=provider, model=model, reasoning=effort, options=options, harness=harness,
                reasoning_configuration_reference="discovered-" + hashlib.sha256(identity).hexdigest()[:32] + "@1",
                preference_only=True, source=source, observed_at=observed)


def public_choices(catalog, observed):
    result = []
    for provider, upstream, harness in [("anthropic", "anthropic", "claude"), ("openai", "openai", "codex"), ("google", "google", "gemini"), ("opencode", "opencode", "opencode"), ("longcat", "opencode", "opencode"), ("nemotron", "opencode", "opencode")]:
        for identity, model in catalog.get(upstream, {}).get("models", {}).items():
            if not model.get("tool_call") or "text" not in model.get("modalities", {}).get("input", ["text"]):
                continue
            if provider in {"longcat", "nemotron"} and provider not in identity.lower():
                continue
            name = "opencode/" + identity if upstream == "opencode" else identity
            source = "OpenCode public catalog (access unverified)"
            controls = []
            for control in model.get("reasoning_options", []):
                if control["type"] == "effort":
                    key = {"anthropic": "thinking", "google": "thinking_level"}.get(provider, "reasoning_effort")
                    controls.extend((level, {key: level}) for level in control.get("values", []))
                elif control["type"] == "budget_tokens":
                    controls.extend((f"Budget {budget:,}", {"thinking_budget": budget}) for budget in sorted({control[k] for k in ("min", "max") if isinstance(control.get(k), int)}))
                elif control["type"] == "toggle" and provider not in {"anthropic", "google"}:
                    controls.extend([("Thinking on", {"think": True}), ("Thinking off", {"think": False})])
            for effort, options in controls or [("Provider default", {})]:
                result.append(choice(provider, name, effort, options, harness, source, observed))
    return result


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    raw = (json.dumps(value, indent=2) + "\n").encode()
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        temporary.write(raw)
        temporary.flush()
        os.fsync(temporary.fileno())
    os.replace(temporary.name, path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--claude-sdk", required=True, type=Path, help="Installed official SDK's sdk.mjs")
    parser.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    args = parser.parse_args()
    observed = datetime.datetime.now(datetime.timezone.utc).isoformat()
    helper = Path(__file__).with_name("model-catalog-claude.mjs")
    state_path = args.output.with_name("model-catalog-sources.json")
    previous = json.loads(state_path.read_text()) if state_path.exists() else {}
    with tempfile.TemporaryDirectory(prefix="aq-model-metadata-") as cwd:
        jobs = {
            "codex": lambda: codex_models(cwd),
            "claude": lambda: json.loads(subprocess.check_output(["node", str(helper), str(args.claude_sdk.resolve()), "claude"], cwd=cwd, stderr=subprocess.DEVNULL, timeout=35)),
            "gemini": lambda: gemini_models(cwd),
            "public": lambda: fetch("https://models.opencode.ai/api.json"),
            "ollama": lambda: [{**m, "details": fetch(args.ollama_url.rstrip("/") + "/api/show", {"model": m["name"]})} for m in fetch(args.ollama_url.rstrip("/") + "/api/tags")["models"]],
        }
        with concurrent.futures.ThreadPoolExecutor(max_workers=5) as executor:
            pending = {executor.submit(job): name for name, job in jobs.items()}
            for future in concurrent.futures.as_completed(pending):
                name = pending[future]
                try:
                    previous[name] = {"observed_at": observed, "data": future.result(), "status": "fresh"}
                except Exception as error:
                    previous.setdefault(name, {"data": [], "observed_at": None})["status"] = "unavailable: " + type(error).__name__
    choices = public_choices(previous.get("public", {}).get("data", {}) or {}, previous.get("public", {}).get("observed_at"))
    for model in previous.get("codex", {}).get("data", []):
        levels = [(e["reasoningEffort"], {"reasoning_effort": e["reasoningEffort"]}) for e in model.get("supportedReasoningEfforts", [])]
        for level, options in levels or [("Provider default (levels not reported)", {})]:
            choices.append(choice("openai", model["model"], level, options, "codex", "Codex account catalog", previous["codex"]["observed_at"]))
    for model in previous.get("claude", {}).get("data", []):
        name = model.get("resolvedModel", model["value"]) if model["value"] in {"default", "opus", "sonnet", "haiku"} else model["value"]
        levels = [(level, {"thinking": level}) for level in model.get("supportedEffortLevels", [])]
        for level, options in levels or [("Provider default (levels not reported)", {})]:
            choices.append(choice("anthropic", name, level, options, "claude", "Claude Code account catalog", previous["claude"]["observed_at"]))
    for model in previous.get("gemini", {}).get("data", []):
        name = model["modelId"]
        existing = [c for c in choices if c["provider"] == "google" and c["model"] == name]
        for c in existing or [choice("google", name, "Provider default (levels not reported)", {}, "gemini", "Gemini CLI account catalog", previous["gemini"]["observed_at"])]:
            c = dict(c, source="Gemini CLI account catalog" + ("; effort metadata: OpenCode" if existing else ""))
            choices.append(c)
    for model in previous.get("ollama", {}).get("data", []):
        caps = model["details"].get("capabilities", [])
        if "completion" not in caps:
            continue
        values = model["details"].get("thinking", {}).get("values", [])
        levels = [("Thinking on" if value is True else "Thinking off" if value is False else str(value), {"think": value}) for value in values if isinstance(value, (str, bool))]
        if not levels:
            levels = [("Provider default (levels not reported)", {})]
        for level, options in levels:
            choices.append(choice("ollama", "ollama/" + model["name"], level, options, "opencode", "Local Ollama API", previous["ollama"]["observed_at"]))
    for cached in choices:
        key = next((key for prefix, key in [("Codex", "codex"), ("Claude", "claude"), ("Gemini", "gemini"), ("Local Ollama", "ollama"), ("OpenCode", "public")] if cached["source"].startswith(prefix)), None)
        if key and previous.get(key, {}).get("status") != "fresh":
            cached["source"] += " (last known; refresh unavailable)"
    imported = args.output.with_name("imported-model-choices.json")
    if imported.exists():
        choices.extend(json.loads(imported.read_text()))
    # Prefer official client facts when the same model/options appear in public metadata.
    unique = {(c["provider"], c["model"], c["reasoning_configuration_reference"], c["harness"]): c for c in choices}
    result = sorted(unique.values(), key=lambda c: (c["provider"], c["model"], c["reasoning"]))
    if not result or len(result) > 4096 or len(json.dumps(result).encode()) > 2_097_152:
        raise ValueError("catalog empty or exceeds Settings limits; old cache preserved")
    atomic_json(state_path, previous)
    atomic_json(args.output, result)
    print(json.dumps({"choices": len(result), "models": len({(c["provider"], c["model"]) for c in result}), "providers": sorted({c["provider"] for c in result}), "sources": {k: v["status"] for k, v in previous.items()}}))


if __name__ == "__main__":
    main()
