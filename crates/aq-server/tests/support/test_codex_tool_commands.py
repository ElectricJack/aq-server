"""Pure synthetic-transcript tests; no process, worker or provider is started."""
import copy
import hashlib
import io
import json
import types
import unittest
from unittest.mock import patch

import codex_tool_commands as tools


def binding():
    return {"schema_version": 1, "launch": {
        "schema_version": 1, "offer_id": "00000000-0000-0000-0000-000000000004",
        "attempt_id": "00000000-0000-0000-0000-000000000005", "assignment_generation": 1,
        "session": {"kind": "remote", "node_id": "00000000-0000-0000-0000-000000000006",
                    "boot_id": "00000000-0000-0000-0000-000000000007",
                    "attempt_id": "00000000-0000-0000-0000-000000000005",
                    "assignment_generation": 1,
                    "session_id": "00000000-0000-0000-0000-000000000008"},
        "execution_spec_digest": "a" * 64, "launch_manifest_digest": "b" * 64,
        "graph_access_profile_digest": "c" * 64},
        "execution_record_digest": "d" * 64,
        "expected_outcome": {"id": "aq.expected-outcome", "version": "1", "digest": "e" * 64},
        "operation_id": "00000000-0000-0000-0000-000000000001",
        "report_id": "00000000-0000-0000-0000-000000000002",
        "nonce": "00000000-0000-0000-0000-000000000003"}


def digest(value):
    # Independent expected serialization, matching the shared canonical contract.
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(raw.encode()).hexdigest()


def view(b, kind="accepted"):
    phase = {"kind": kind}
    if kind == "recorded":
        phase["receipt"] = {"id": "aq.result-record/" + b["report_id"],
                            "version": "1", "digest": "f" * 64}
    return {"contract": "aq-agent-result-local-1", "operation_id": b["operation_id"],
            "report_id": b["report_id"], "binding_digest": digest(b),
            "submission_digest": digest({"schema_version": 1, "binding": b,
                                          "conclusion": tools.CONCLUSION}),
            "expected_outcome": b["expected_outcome"], "phase": phase,
            "owner_present": kind != "recorded", "projection": "local_result_data_only"}


def submit_text(b, submitted=None, status=None):
    submitted = view(b) if submitted is None else submitted
    status = view(b) if status is None else status
    return "\n".join([
        "AQ_GATE04_SUBMIT_BEGIN", json.dumps(submitted),
        "AQ_GATE04_SUBMIT_EXIT=" + ("0" if submitted["phase"]["kind"] == "recorded" else "10"),
        "AQ_GATE04_STATUS_BEGIN", json.dumps(status),
        "AQ_GATE04_STATUS_EXIT=" + ("0" if status["phase"]["kind"] == "recorded" else "10"),
        "AQ_GATE04_RESULT_TOOL_DONE",
    ])


def output(call, text):
    return {"type": "function_call_output", "call_id": call, "output": text}


def request(*items):
    return {"input": list(items)}


def normal(b, submitted=None, status=None):
    edit = output(tools.EDIT_CALL_ID, "Process exited with code 0\nAQ_GATE04_EDIT_CHECK_OK\n")
    submit = output(tools.SUBMIT_CALL_ID, submit_text(b, submitted, status))
    return [request(), request(edit), request(edit, submit)]


def poll_call(description):
    return {"type": "function_call", "call_id": description["call_id"],
            "name": description["tool"], "arguments": json.dumps(description["arguments"])}


class ToolCommands(unittest.TestCase):
    def test_fixed_commands_use_only_scoped_runtime_channel_and_closed_claim(self):
        vectors = tools.vectors("fixture-model")
        self.assertEqual(vectors["operation"], "vectors")
        self.assertEqual(len(vectors["commands"]), 2)
        self.assertIn("set -euC", tools.edit_command())
        self.assertIn("/usr/bin/cmp -s - gate04-proof.txt", tools.edit_command())
        self.assertIn("AQ_AGENT_RESULT_COMMAND", tools.submit_command())
        self.assertIn("AQ_AGENT_RESULT_DIR/submissions/request.json", tools.submit_command())
        self.assertNotIn("--state-dir", tools.submit_command())
        for forbidden in ("source_receipt_ids", "pending_effect_ids", "nonce", "task close", "stop:"):
            self.assertNotIn(forbidden, tools.submit_command())
        self.assertLessEqual(len(tools.CONCLUSION.encode()), 512)
        self.assertEqual(tools.PROOF_BYTES, b"AQ native result fixture proof\n")

    def test_normal_three_requests_require_actual_ack_and_exact_original_binding(self):
        b = binding()
        requests = normal(b)
        self.assertEqual(tools.next_tool(requests[:1])["call_id"], tools.EDIT_CALL_ID)
        self.assertEqual(tools.next_tool(requests[:2])["call_id"], tools.SUBMIT_CALL_ID)
        self.assertIsNone(tools.next_tool(requests))
        actual = tools.validate_tool_outputs(requests, b)
        self.assertEqual(actual["operation_id"], b["operation_id"])
        self.assertEqual(actual["submission_digest"], view(b)["submission_digest"])
        self.assertEqual(actual["phase"], "accepted")
        self.assertNotIn("nonce", json.dumps(actual))
        self.assertNotIn("completion", tools.apply({"operation": "next_tool", "requests": requests}))
        self.assertEqual(tools.apply({"operation": "validate", "requests": requests,
                                     "binding": b})["completion"], tools.FINAL_TEXT)

    def test_real_observed_session_poll_preserves_split_client_output(self):
        b = binding()
        edit = normal(b)[1]["input"][0]
        start = output(tools.SUBMIT_CALL_ID,
                       "AQ_GATE04_SUBMIT_BEGIN\nProcess running with session ID 77\n")
        requests = [request(), request(edit), request(edit, start)]
        poll = tools.next_tool(requests)
        self.assertEqual(poll["call_id"], "call_gate04_poll_1")
        self.assertEqual(poll["arguments"]["session_id"], 77)
        rest = submit_text(b).split("\n", 1)[1]
        requests.append(request(edit, start, poll_call(poll), output(poll["call_id"], rest)))
        self.assertIsNone(tools.next_tool(requests))
        self.assertEqual(tools.validate_tool_outputs(requests, b)["phase"], "accepted")
        wrong = copy.deepcopy(requests)
        wrong[-1]["input"][-2]["arguments"] = json.dumps(poll["arguments"] | {"session_id": 78})
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(wrong, b)

    def test_missing_observed_session_and_exhausted_poll_budget_never_invent_ids(self):
        edit = output(tools.EDIT_CALL_ID, "unfinished without session identifier")
        with self.assertRaises(ValueError):
            tools.next_tool([request(), request(edit)])
        edit = output(tools.EDIT_CALL_ID, "Process running with session ID 22")
        requests = [request(), request(edit)]
        items = [edit]
        for _ in range(4):
            poll = tools.next_tool(requests)
            items += [poll_call(poll), output(poll["call_id"], "Process running with session ID 22")]
            requests.append(request(*items))
        with self.assertRaises(ValueError):
            tools.next_tool(requests)
        with self.assertRaises(ValueError):
            tools.next_tool([request()] * 8)

    def test_foreign_ids_digests_changed_bytes_and_owner_loss_refuse(self):
        b = binding()
        for field, changed in [("operation_id", b["report_id"]),
                               ("report_id", b["operation_id"]),
                               ("binding_digest", "0" * 64),
                               ("submission_digest", "0" * 64),
                               ("expected_outcome", {"id": "other"}),
                               ("owner_present", False)]:
            status = view(b)
            status[field] = changed
            with self.subTest(field=field), self.assertRaises(ValueError):
                tools.validate_tool_outputs(normal(b, status=status), b)
        changed = copy.deepcopy(b)
        changed["nonce"] = "00000000-0000-0000-0000-000000000009"
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(normal(b), changed)

    def test_pending_alone_is_not_ack_but_pending_then_matching_ack_is_valid(self):
        b = binding()
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(normal(b, status=view(b, "pending_local")), b)
        self.assertEqual(tools.validate_tool_outputs(
            normal(b, submitted=view(b, "pending_local"), status=view(b, "finalizing")), b)["phase"],
            "finalizing")
        recorded = view(b, "recorded")
        self.assertEqual(tools.validate_tool_outputs(normal(b, status=recorded), b)["phase"], "recorded")
        recorded["phase"]["receipt"]["id"] = "aq.result-record/other"
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(normal(b, status=recorded), b)

    def test_repeated_history_is_allowed_but_changed_output_or_extra_privilege_refuses(self):
        b = binding()
        changed = normal(b)
        changed[-1]["input"][0] = copy.deepcopy(changed[-1]["input"][0])
        changed[-1]["input"][0]["output"] += "changed"
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(changed, b)
        status = view(b)
        status["source_receipt_ids"] = ["never supplied by the agent"]
        with self.assertRaises(ValueError):
            tools.validate_tool_outputs(normal(b, status=status), b)

    def test_cli_closed_fields_duplicate_refusal_bounds_and_redacted_error(self):
        with self.assertRaises(ValueError):
            tools.apply({"operation": "vectors", "model": "m", "execute": True})
        with self.assertRaises(ValueError):
            tools._json('{"operation":"vectors","operation":"validate"}')
        for raw in [b'{"operation":"unknown","secret":"private-value"}',
                    b" " * (tools.MAX_INPUT_BYTES + 1)]:
            out, error = io.StringIO(), io.StringIO()
            with patch.object(tools.sys, "stdin", types.SimpleNamespace(buffer=io.BytesIO(raw))), \
                    patch.object(tools.sys, "stdout", out), patch.object(tools.sys, "stderr", error):
                code = tools.main()
            self.assertEqual(code, 2)
            self.assertEqual(out.getvalue(), "")
            self.assertNotIn("private-value", error.getvalue())
            self.assertEqual(json.loads(error.getvalue())["code"], "invalid_finite_tool_fixture")

    def test_cumulative_call_definitions_preserve_identity_and_fixed_command(self):
        command = tools.next_tool([request()])
        definition = poll_call(command)
        edit = normal(binding())[1]["input"][0]
        repeated = copy.deepcopy(definition)
        repeated["arguments"] = command["arguments"]
        requests = [request(definition), request(repeated, edit)]
        self.assertEqual(tools.next_tool(requests)["call_id"], tools.SUBMIT_CALL_ID)
        repeated["arguments"] = command["arguments"] | {"cmd": "different command"}
        with self.assertRaisesRegex(ValueError, "changed tool call identity"):
            tools.next_tool(requests)
        with self.assertRaisesRegex(ValueError, "fixture command differs"):
            tools.next_tool([request(repeated, edit)])

    def test_submit_requires_completed_edit_and_no_pending_tool(self):
        submit = output(tools.SUBMIT_CALL_ID, submit_text(binding()))
        with self.assertRaisesRegex(ValueError, "submit preceded genuine edit/check output"):
            tools.next_tool([request(submit)])
        edit = output(tools.EDIT_CALL_ID, "Process running with session ID 22")
        with self.assertRaisesRegex(ValueError, "another fixture tool is still pending"):
            tools.next_tool([request(edit, submit)])

    def test_exec_definitions_require_original_tool_and_command_for_both_turns(self):
        b = binding()
        for index, call in [(0, tools.EDIT_CALL_ID), (1, tools.SUBMIT_CALL_ID)]:
            for change in ({"tool": "write_stdin"}, {"arguments": {}},
                           {"arguments": {"cmd": "different command"}}):
                requests = normal(b)
                command = tools.next_tool(requests[:index + 1])
                definition = poll_call(command | change)
                # Cumulative histories may omit definitions; validate any
                # definition that is supplied alongside its observed output.
                requests[-1]["input"].insert(index, definition)
                with self.subTest(call=call, change=change), \
                        self.assertRaisesRegex(ValueError, "fixture command differs"):
                    tools.validate_tool_outputs(requests, b)

    def test_poll_requires_definition_exact_arguments_and_sequential_call_id(self):
        edit = output(tools.EDIT_CALL_ID, "Process running with session ID 22")
        requests = [request(), request(edit)]
        poll = tools.next_tool(requests)
        for change in ({"tool": "exec_command"},
                       {"arguments": poll["arguments"] | {"chars": "input"}},
                       {"arguments": poll["arguments"] | {"yield_time_ms": 2000}},
                       {"arguments": poll["arguments"] | {"max_output_tokens": 4096}},
                       {"arguments": poll["arguments"] | {"extra": True}}):
            items = [edit, poll_call(poll | change),
                     output(poll["call_id"], "AQ_GATE04_EDIT_CHECK_OK")]
            with self.subTest(change=change), \
                    self.assertRaisesRegex(ValueError, "poll differs from observed session"):
                tools.next_tool(requests + [request(*items)])
        with self.assertRaisesRegex(ValueError, "poll differs from observed session"):
            tools.next_tool(requests + [request(
                edit, output(poll["call_id"], "AQ_GATE04_EDIT_CHECK_OK"))])
        with self.assertRaisesRegex(ValueError, "poll has no observed original session"):
            tools.next_tool(requests + [request(
                edit, output("call_gate04_poll_2", "AQ_GATE04_EDIT_CHECK_OK"))])

    def test_session_identity_must_be_unambiguous_bounded_and_stable(self):
        for text, message in [
            ("Process running with session ID 0", "changed tool session"),
            (f"Process running with session ID {2**31}", "changed tool session"),
            ("Process running with session ID 22\nProcess running with session ID 23",
             "ambiguous tool session"),
        ]:
            with self.subTest(text=text), self.assertRaisesRegex(ValueError, message):
                tools.next_tool([request(output(tools.EDIT_CALL_ID, text))])
        edit = output(tools.EDIT_CALL_ID, "Process running with session ID 22")
        requests = [request(), request(edit)]
        poll = tools.next_tool(requests)
        requests.append(request(edit, poll_call(poll), output(
            poll["call_id"], "Process running with session ID 23")))
        with self.assertRaisesRegex(ValueError, "changed tool session"):
            tools.next_tool(requests)

    def test_client_phase_receipt_and_exit_code_remain_closed(self):
        b = binding()
        for kind, field, changed in [
            ("accepted", "extra", True),
            ("recorded", "extra", True),
            ("recorded", "receipt", {"id": "other", "version": "1", "digest": "f" * 64}),
            ("recorded", "receipt", {"id": "aq.result-record/" + b["report_id"],
                                     "version": "2", "digest": "f" * 64}),
            ("recorded", "receipt", {"id": "aq.result-record/" + b["report_id"],
                                     "version": "1", "digest": "F" * 64}),
        ]:
            status = view(b, kind)
            status["phase"][field] = changed
            with self.subTest(kind=kind, field=field, changed=changed), self.assertRaises(ValueError):
                tools.validate_tool_outputs(normal(b, status=status), b)
        for kind, valid_code, wrong_code in [("accepted", "10", "0"), ("recorded", "0", "10")]:
            requests = normal(b, status=view(b, kind))
            submit = requests[-1]["input"][-1]
            submit["output"] = submit["output"].replace(
                "AQ_GATE04_STATUS_EXIT=" + valid_code, "AQ_GATE04_STATUS_EXIT=" + wrong_code)
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                tools.validate_tool_outputs(requests, b)


if __name__ == "__main__":
    unittest.main()
