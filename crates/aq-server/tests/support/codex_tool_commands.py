"""Finite tool data/assertions only; no listener, process or live authority.

The controller owns the real worker, request admission, shared SSE builder,
original deadline and cleanup. Synthetic unit-test transcripts are not kernel,
source-retention, check, review or model qualification.
"""
import hashlib
import json
import re
import shlex
import sys
import uuid

PROOF_PATH = "gate04-proof.txt"
PROOF_BYTES = b"AQ native result fixture proof\n"
CONCLUSION = (
    "Changes: created gate04-proof.txt; Checks: /usr/bin/cmp -s - "
    "gate04-proof.txt exit0; Limits: finite local peer, no provider/check/review claim"
)
EDIT_CALL_ID = "call_gate04_edit"
SUBMIT_CALL_ID = "call_gate04_submit"
FINAL_TEXT = "AQ finite tool sequence ended; worker finalization remains separate."
MAX_REQUESTS = 7
MAX_POLLS = 4
_EDIT_DONE = "AQ_GATE04_EDIT_CHECK_OK"
_DONE = "AQ_GATE04_RESULT_TOOL_DONE"
_RUNNING = re.compile(r"Process running with session ID[: ]+([0-9]+)", re.I)
_POLL = re.compile(r"call_gate04_poll_([1-4])\Z")
MAX_INPUT_BYTES = 8 * 1024 * 1024


def _require(condition, message):
    if not condition:
        raise ValueError(message)


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode("utf-8")


def _digest(value):
    return hashlib.sha256(_canonical(value)).hexdigest()


def _json(text):
    def unique(pairs):
        value = {}
        for key, item in pairs:
            _require(key not in value, "duplicate JSON field")
            value[key] = item
        return value

    def invalid_constant(_):
        raise ValueError("nonfinite JSON number")

    return json.loads(text, object_pairs_hook=unique, parse_constant=invalid_constant)


def edit_command():
    # Create-only, fixed relative fixture member. Never overwrite an existing
    # user file or follow a preexisting symlink, and actually check its bytes.
    return "\n".join([
        "set -euC",
        "/usr/bin/printf '%s\\n' 'AQ native result fixture proof' > gate04-proof.txt",
        "/usr/bin/printf '%s\\n' 'AQ native result fixture proof' | /usr/bin/cmp -s - gate04-proof.txt",
        f"/usr/bin/printf '%s\\n' '{_EDIT_DONE}'",
    ])


def submit_command():
    request = json.dumps({"schema_version": 1, "conclusion": CONCLUSION},
                         separators=(",", ":"))
    _require(len(CONCLUSION.encode()) <= 512, "fixture conclusion byte bound")
    return "\n".join([
        "set -euC", "umask 077",
        '[ -n "${AQ_AGENT_RESULT_DIR:-}" ] && [ -n "${AQ_AGENT_RESULT_COMMAND:-}" ]',
        "/usr/bin/printf '%s\\n' " + shlex.quote(request)
        + ' > "$AQ_AGENT_RESULT_DIR/submissions/request.json"',
        "/usr/bin/printf '%s\\n' 'AQ_GATE04_SUBMIT_BEGIN'", "set +e",
        '"$AQ_AGENT_RESULT_COMMAND" result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"',
        "submitted=$?", "set -e",
        "/usr/bin/printf 'AQ_GATE04_SUBMIT_EXIT=%s\\n' \"$submitted\"",
        '[ "$submitted" -eq 10 ] || [ "$submitted" -eq 0 ]',
        "/usr/bin/printf '%s\\n' 'AQ_GATE04_STATUS_BEGIN'", "set +e",
        '"$AQ_AGENT_RESULT_COMMAND" result status', "observed=$?", "set -e",
        "/usr/bin/printf 'AQ_GATE04_STATUS_EXIT=%s\\n' \"$observed\"",
        '[ "$observed" -eq 10 ] || [ "$observed" -eq 0 ]',
        f"/usr/bin/printf '%s\\n' '{_DONE}'",
    ])


def _exec(call_id, command):
    label = "edit" if call_id == EDIT_CALL_ID else "submit"
    return {"response_id": f"resp_gate04_{label}",
            "item_id": f"item_gate04_{label}", "call_id": call_id,
            "tool": "exec_command", "arguments": {
                "cmd": command, "login": False, "yield_time_ms": 1000,
                "max_output_tokens": 2048}}


def vectors(model):
    """Normal two-tool-turn data for the EXISTING fixture-responses exporter.

    The controller uses next_tool for observed asynchronous session polling;
    this convenience value does not promise an exact request count.
    """
    _require(isinstance(model, str) and 0 < len(model) <= 128
             and not any(ord(c) < 32 for c in model), "bounded fixture model")
    commands = [_exec(EDIT_CALL_ID, edit_command()),
                _exec(SUBMIT_CALL_ID, submit_command())]
    return {"operation": "vectors", "model": model, "commands": [
        {key: value for key, value in command.items()
         if key in ("response_id", "item_id", "call_id")}
        | {"command": command["arguments"]["cmd"]} for command in commands],
        "completion": FINAL_TEXT}


def _output(value):
    if isinstance(value, str):
        result = value
    elif isinstance(value, list):
        _require(len(value) <= 16 and all(isinstance(v, dict)
                 and v.get("type") in ("input_text", "text")
                 and isinstance(v.get("text"), str) for v in value),
                 "unsupported tool output blocks")
        result = "\n".join(v["text"] for v in value)
    else:
        raise ValueError("unsupported tool output shape")
    _require(len(result.encode()) <= 65536, "tool output byte bound")
    return result


def _tool_history(requests):
    """Deduplicate cumulative request history without losing first-output order."""
    _require(isinstance(requests, list) and 1 <= len(requests) <= MAX_REQUESTS,
             "finite request count")
    outputs, definitions = {}, {}
    order = []
    for request in requests:
        items = request.get("input") if isinstance(request, dict) else None
        _require(isinstance(items, list) and len(items) <= 1024,
                 "bounded request input")
        for item in items:
            if not isinstance(item, dict) or item.get("type") not in (
                    "function_call", "function_call_output"):
                continue
            call = item.get("call_id")
            _require(call in (EDIT_CALL_ID, SUBMIT_CALL_ID)
                     or isinstance(call, str) and _POLL.fullmatch(call),
                     "uncontrolled fixture tool call")
            if item["type"] == "function_call":
                args = item.get("arguments")
                args = _json(args) if isinstance(args, str) else args
                _require(isinstance(args, dict), "tool argument object required")
                definition = (item.get("name"), args)
                _require(call not in definitions or definitions[call] == definition,
                         "changed tool call identity")
                definitions[call] = definition
            else:
                text = _output(item.get("output"))
                _require(call not in outputs or outputs[call] == text,
                         "changed repeated tool output")
                if call not in outputs:
                    order.append(call)
                outputs[call] = text
    return outputs, definitions, order


def _observe_session(text, sessions, active):
    observed = _RUNNING.findall(text)
    if not observed:
        return
    _require(len(set(observed)) == 1, "ambiguous tool session")
    session = int(observed[0])
    _require(0 < session <= 2**31 - 1
             and (active not in sessions or sessions[active] == session),
             "changed tool session")
    sessions[active] = session


def _trace(requests):
    outputs, definitions, order = _tool_history(requests)
    streams = {EDIT_CALL_ID: "", SUBMIT_CALL_ID: ""}
    sessions = {}
    active = None
    polls = 0
    for call in order:
        text = outputs[call]
        if call in streams:
            _require(not active, "another fixture tool is still pending")
            if call == SUBMIT_CALL_ID:
                _require(_EDIT_DONE in streams[EDIT_CALL_ID].splitlines(),
                         "submit preceded genuine edit/check output")
            active = call
            if call in definitions:
                _require(definitions[call][0] == "exec_command"
                         and definitions[call][1].get("cmd") == (
                             edit_command() if call == EDIT_CALL_ID else submit_command()),
                         "fixture command differs")
        else:
            polls += 1
            _require(call == f"call_gate04_poll_{polls}" and active in sessions,
                     "poll has no observed original session")
            # A poll is admitted only for the actual session returned by the
            # controlled tool, never an invented/fallback process identifier.
            _require(call in definitions
                     and definitions[call] == ("write_stdin", {
                         "session_id": sessions[active], "chars": "",
                         "yield_time_ms": 1000, "max_output_tokens": 2048}),
                     "poll differs from observed session")
        streams[active] += text + "\n"
        _observe_session(text, sessions, active)
        done = _EDIT_DONE if active == EDIT_CALL_ID else _DONE
        if done in streams[active].splitlines():
            active = None
    return streams, sessions, active, polls


def next_tool(requests):
    """Return finite command data only; parent owns delivery and lifecycle."""
    streams, sessions, active, polls = _trace(requests)
    if active:
        _require(active in sessions and polls < MAX_POLLS,
                 "tool unfinished without a bounded observed session")
        _require(len(requests) < MAX_REQUESTS, "fixture request budget exhausted")
        number = polls + 1
        return {"response_id": f"resp_gate04_poll_{number}",
                "item_id": f"item_gate04_poll_{number}",
                "call_id": f"call_gate04_poll_{number}", "tool": "write_stdin",
                "arguments": {"session_id": sessions[active], "chars": "",
                              "yield_time_ms": 1000, "max_output_tokens": 2048}}
    if not streams[EDIT_CALL_ID]:
        _require(len(requests) == 1, "missing original edit output")
        return _exec(EDIT_CALL_ID, edit_command())
    if not streams[SUBMIT_CALL_ID]:
        return _exec(SUBMIT_CALL_ID, submit_command())
    return None


def _block(lines, start, end):
    _require(lines.count(start) == 1, "missing/duplicate CLI block")
    exits = [i for i, line in enumerate(lines) if line.startswith(end + "=")]
    _require(len(exits) == 1 and lines.index(start) < exits[0], "CLI exit marker")
    body = lines[lines.index(start) + 1:exits[0]]
    documents = [_json(line) for line in body if line.startswith("{")]
    _require(len(documents) == 1 and isinstance(documents[0], dict),
             "exact client JSON output required")
    code = int(lines[exits[0]].split("=", 1)[1])
    return documents[0], code


def _binding_digests(binding):
    _require(isinstance(binding, dict) and set(binding) == {
        "schema_version", "launch", "execution_record_digest", "expected_outcome",
        "operation_id", "report_id", "nonce"} and binding["schema_version"] == 1,
        "original worker binding required")
    ids = [uuid.UUID(binding[key]) for key in ("operation_id", "report_id", "nonce")]
    _require(all(ids) and all(i.int for i in ids) and len(set(ids)) == 3,
             "original binding UUIDs")
    return _digest(binding), _digest({"schema_version": 1, "binding": binding,
                                     "conclusion": CONCLUSION})


def _validate_client_scope(value, binding, scope_digest, submission_digest):
    expected_keys = {"contract", "operation_id", "report_id", "binding_digest",
                     "expected_outcome", "submission_digest", "phase", "owner_present", "projection"}
    _require(set(value) == expected_keys
             and value["contract"] == "aq-agent-result-local-1"
             and value["operation_id"] == binding["operation_id"]
             and value["report_id"] == binding["report_id"]
             and value["binding_digest"] == scope_digest
             and value["expected_outcome"] == binding["expected_outcome"]
             and value["submission_digest"] == submission_digest
             and value["projection"] == "local_result_data_only"
             and isinstance(value["owner_present"], bool),
             "client output differs from original claim scope")


def _validate_client_phase(value, code, report_id, allow_pending):
    phase = value["phase"]
    kind = phase.get("kind") if isinstance(phase, dict) else None
    if kind == "recorded":
        receipt = phase.get("receipt")
        _require(code == 0 and set(phase) == {"kind", "receipt"}
                 and isinstance(receipt, dict) and set(receipt) == {"id", "version", "digest"}
                 and receipt["id"] == "aq.result-record/" + report_id
                 and receipt["version"] == "1"
                 and re.fullmatch(r"[0-9a-f]{64}", receipt["digest"]),
                 "recorded reference mismatch")
    else:
        allowed = ("accepted", "finalizing", "pending_local") if allow_pending else ("accepted", "finalizing")
        _require(kind in allowed and set(phase) == {"kind"}
                 and code == 10 and value["owner_present"] is True,
                 "worker acknowledgment absent or owner lost")


def validate_tool_outputs(requests, binding):
    """Check original client data against the independently read actual Binding.

    This never constructs/accepts execution, stopped, source or review authority.
    The controller still verifies real journal/B1/source owners and physical stop.
    """
    streams, _, active, _ = _trace(requests)
    _require(active is None and _EDIT_DONE in streams[EDIT_CALL_ID].splitlines()
             and _DONE in streams[SUBMIT_CALL_ID].splitlines(),
             "controlled tool sequence incomplete")
    scope_digest, submission_digest = _binding_digests(binding)
    lines = streams[SUBMIT_CALL_ID].splitlines()
    submitted, submit_exit = _block(lines, "AQ_GATE04_SUBMIT_BEGIN", "AQ_GATE04_SUBMIT_EXIT")
    status, status_exit = _block(lines, "AQ_GATE04_STATUS_BEGIN", "AQ_GATE04_STATUS_EXIT")
    for value, code, allow_pending in [(submitted, submit_exit, True), (status, status_exit, False)]:
        _validate_client_scope(value, binding, scope_digest, submission_digest)
        _validate_client_phase(value, code, binding["report_id"], allow_pending)
    return {"operation_id": binding["operation_id"], "report_id": binding["report_id"],
            "binding_digest": scope_digest, "submission_digest": submission_digest,
            "phase": status["phase"]["kind"], "projection": "local_result_data_only"}


def apply(value):
    """Closed, data-only stdin/stdout adapter for the Rust parent."""
    _require(isinstance(value, dict), "closed fixture operation required")
    operation = value.get("operation")
    if operation == "vectors":
        _require(set(value) == {"operation", "model"}, "closed vectors request")
        return vectors(value["model"])
    if operation == "next_tool":
        _require(set(value) == {"operation", "requests"}, "closed next-tool request")
        return {"next": next_tool(value["requests"])}
    if operation == "validate":
        _require(set(value) == {"operation", "requests", "binding"},
                 "closed validation request")
        return validate_tool_outputs(value["requests"], value["binding"]) | {
            "completion": FINAL_TEXT}
    raise ValueError("unsupported fixture operation")


def main():
    try:
        data = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
        _require(len(data) <= MAX_INPUT_BYTES, "fixture adapter byte bound")
        output = apply(_json(data))
        sys.stdout.write(json.dumps(output, separators=(",", ":"), allow_nan=False) + "\n")
        return 0
    except Exception:
        # Never dump input, filesystem paths, local binding/nonce or credentials.
        sys.stderr.write('{"code":"invalid_finite_tool_fixture","effect":"not_asserted"}\n')
        return 2


if __name__ == "__main__":
    sys.exit(main())
