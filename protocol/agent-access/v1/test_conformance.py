"""Offline API-01 conformance; no daemon, graph, worker or credentials required."""
from copy import deepcopy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest

from jsonschema import Draft202012Validator, ValidationError

import build_fixtures
import generate
from conformance_model import (GraphServiceFixture, OperationLedger, Refused, assert_read_response, assert_receipt,
                               authorize_batch, authorize_read, project_context, readable, retry_action,
                               scoped_edge, scoped_node, translate_fixture)
from validate import SCHEMA, canonical, digest, strict_loads, upstream_validate, validate

ROOT = Path(__file__).resolve().parent
BASE = json.loads((ROOT / 'fixtures/base.json').read_text())
GRAPH = json.loads((ROOT / 'fixtures/qualified-graph.json').read_text())
uid = build_fixtures.uid


class Conformance(unittest.TestCase):
    def setUp(self):
        self.base = deepcopy(BASE)
        self.context = self.base['context']
        self.agent = self.base['agent_principal']
        self.graph = deepcopy(GRAPH)

    def refused(self, code, call, *args):
        with self.assertRaises(Refused) as found:
            call(*args)
        self.assertEqual(found.exception.code, code)

    def test_generated_public_files_and_types_have_no_drift(self):
        for name, expected in generate.render().items():
            self.assertEqual((ROOT / name).read_text(), expected, name)
        spec = importlib.util.spec_from_file_location('generated_agent_types', ROOT / 'client_types.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertIn('commands', module.CommandRequest.__annotations__)
        Draft202012Validator.check_schema(SCHEMA)
        openapi = json.loads((ROOT / 'openapi.json').read_text())
        self.assertEqual(len(openapi['paths']), 12)
        for path in ['/v1/agent/messages/send', '/v1/agent/messages/inbox', '/v1/agent/messages/ack', '/v1/agent/messages/body', '/v2/worker/messages/poll', '/v2/worker/messages/ack']:
            self.assertIn(path, openapi['paths'])
        self.assertEqual(openapi['paths']['/v2/worker/agent-operations/query']['post']['security'], [{'NodeRecoveryBearer': []}])

    def test_retained_upstream_sources_and_actual_http_probe(self):
        manifest = json.loads((ROOT / 'upstream/manifest.json').read_text())
        for file, expected in manifest['files'].items():
            self.assertEqual(hashlib.sha256((ROOT / 'upstream' / file).read_bytes()).hexdigest(), expected)
        self.assertEqual(manifest['source_revision'], GRAPH['origin']['source_revision'])
        for node in GRAPH['nodes'].values():
            upstream_validate('Node', node)
        for edge in GRAPH['edges'].values():
            upstream_validate('Edge', edge)
        for name, field in [('Context', 'context'), ('RevisionSnapshot', 'revision'), ('HistoryPage', 'history'), ('MutationResult', 'mutation_receipt')]:
            upstream_validate(name, GRAPH[field])
        self.assertEqual(GRAPH['guard_results'], {'valid_same_transaction': 200, 'identical_replay': True,
            'guard_noop_does_not_advance_version': True, 'stale_guard_atomic_refusal': 409,
            'foreign_author_refused': 403, 'retired_source_refused': 403, 'failed_edges_absent': True})

    def test_offline_fixture_generation_is_reproducible(self):
        for file, value in build_fixtures.build().items():
            self.assertEqual(json.loads((ROOT / 'fixtures' / file).read_text()), value)

    def test_published_valid_and_invalid_wire_vectors_execute(self):
        vectors = json.loads((ROOT / 'fixtures/messages.json').read_text())
        self.assertGreaterEqual(sum(x['valid'] for x in vectors), 26)
        for vector in vectors:
            with self.subTest(vector=vector['name']):
                if vector['valid']:
                    validate(vector['type'], vector['value'])
                else:
                    with self.assertRaises((ValueError, ValidationError)):
                        validate(vector['type'], vector['value'])

    def test_portable_scope_vectors_execute(self):
        cases = json.loads((ROOT / 'fixtures/scope-cases.json').read_text())
        for case in cases:
            with self.subTest(case=case['name']):
                principal = deepcopy(self.agent);principal.update(kind=case['principal_kind'], active=case['active'])
                action = {'read': authorize_read, 'command': authorize_batch}.get(case['action'])
                if action is None:
                    call = lambda: assert_read_response(case['request'], case['response'], self.context, self.graph)
                else:
                    call = lambda: action(principal, self.context, case['request'], self.graph)
                if case['expected'] == 'allowed':
                    call()
                else:
                    self.refused(case['expected'], call)

    def test_schema_rejects_duplicate_json_float_nil_and_utf8_overrun(self):
        for text in ['{"operation_id":1,"operation_id":2}', '{"x":1.25}', '{"x":NaN}']:
            with self.assertRaises(ValueError):
                strict_loads(text)
        request = deepcopy(self.base['command_requests']['create_finding'])
        request['operation_id'] = uid(0)
        with self.assertRaises(ValidationError):
            validate('CommandRequest', request)
        request = deepcopy(self.base['command_requests']['create_finding'])
        request['commands'][0]['title'] = 'é' * 257
        with self.assertRaises(ValidationError):
            validate('CommandRequest', request)
        bad_context = deepcopy(self.context);bad_context['credential']['expires_at'] = '2026-02-30T00:00:00Z'
        with self.assertRaises(ValidationError):
            validate('ContextResponse', bad_context)
        request = deepcopy(self.base['command_requests']['correct_record'])
        request['commands'][0]['expected_version'] = str(2**63)
        with self.assertRaises(ValidationError):
            validate('CommandRequest', request)

    def test_immutable_profile_context_and_scope_bindings(self):
        validate('ContextResponse', self.context)
        self.context['profile']['role'] = 'administrator'
        with self.assertRaises(ValueError):
            validate('ContextResponse', self.context)
        self.context = deepcopy(BASE['context'])
        self.context['context']['nodes'][0]['body'] += 'changed'
        with self.assertRaises(ValueError):
            validate('ContextResponse', self.context)
        self.context = deepcopy(BASE['context'])
        # Credential rotation changes neither frozen profile nor launch context.
        self.context['credential']['version'] = '2'
        validate('ContextResponse', self.context)
        request = deepcopy(self.base['read_requests']['get'])
        request['binding']['profile_digest'] = '9'*64
        self.refused('stale_binding', authorize_read, self.agent, self.context, request, self.graph)

    def test_nonagent_credentials_cannot_read_or_admit(self):
        request = self.base['read_requests']['get']
        command = self.base['command_requests']['correct_record']
        for kind in ['node', 'worker_runtime', 'graph_service', 'owner']:
            principal = {'kind': kind, 'node_id': uid(100)}
            self.refused('unauthorized', authorize_read, principal, self.context, request, self.graph)
            self.refused('unauthorized', authorize_batch, principal, self.context, command, self.graph)

    def test_expiry_revocation_and_generation_never_admit_new_actions(self):
        request = self.base['command_requests']['correct_record']
        self.agent['active'] = False
        self.refused('session_expired_or_revoked', authorize_batch, self.agent, self.context, request, self.graph)
        self.agent = deepcopy(BASE['agent_principal'])
        self.agent['binding']['generation'] = 2
        self.refused('stale_binding', authorize_batch, self.agent, self.context, request, self.graph)
        self.agent = deepcopy(BASE['agent_principal'])
        changed = deepcopy(request);changed['binding']['session_id'] = uid(999)
        self.refused('stale_binding', authorize_batch, self.agent, self.context, changed, self.graph)

    def test_each_read_query_checks_authority_root_and_response_scope(self):
        for kind, request in self.base['read_requests'].items():
            with self.subTest(kind=kind):
                authorize_read(self.agent, self.context, request, self.graph)
                assert_read_response(request, self.base['read_responses'][kind], self.context, self.graph)
                if 'node_id' in request['query']:
                    bad = deepcopy(request);bad['query']['node_id'] = uid(4)
                    self.refused('not_found', authorize_read, self.agent, self.context, bad, self.graph)
                missing = deepcopy(self.context)
                missing['profile']['capabilities'].remove('read.' + kind)
                self.refused('capability_denied', authorize_read, self.agent, missing, request, self.graph)

    def test_hidden_context_edge_and_pinned_revision_are_filtered_without_counts(self):
        request = self.base['read_requests']['context']
        result = project_context(self.graph['context'], request, self.context, self.graph)
        self.assertNotIn('UNAUTHORIZED-CANARY', canonical(result).decode())
        self.assertNotIn(uid(4), canonical(result).decode())
        self.assertEqual(result['coverage']['status'], 'complete')
        self.assertEqual(set(result), {'kind', 'node', 'nodes', 'edges', 'pinned_revisions', 'coverage'})
        self.assertTrue(any(n['id'] == uid(1) and n['content_revision'] == '1' for n in result['pinned_revisions']))
        self.assertNotIn('total', result)

    def test_scope_checks_every_edge_type_and_both_endpoints(self):
        request = self.base['read_requests']['context']
        for edge_type in generate.EDGE_TYPES:
            raw = deepcopy(self.graph['context'])
            edge = deepcopy(next(iter(self.graph['edges'].values())))
            edge.update(id=uid(900), source=uid(1), target=uid(4), edge_type=edge_type)
            raw['edges'] = [edge]
            projected = project_context(raw, request, self.context, self.graph)
            self.assertEqual(projected['edges'], [], edge_type)
        for source, target in [(uid(4), uid(2)), (uid(2), uid(4))]:
            request = deepcopy(self.base['command_requests']['add_relationship'])
            request['commands'][0].update(source=source, target=target)
            self.refused('not_found', authorize_batch, self.agent, self.context, request, self.graph)

    def test_get_revision_search_history_and_relationships_cannot_leak_hidden_data(self):
        for kind, response in self.base['read_responses'].items():
            bad = deepcopy(response)
            if kind in ('get', 'revision', 'context'):
                bad['result']['node'] = scoped_node(self.graph['nodes'][uid(4)])
            elif kind == 'search':
                bad['result']['nodes'] = [scoped_node(self.graph['nodes'][uid(4)])]
            elif kind == 'history':
                bad['result']['items'][0]['subject_id'] = uid(4)
            else:
                bad['result']['edges'] = [scoped_edge(self.graph['edges'][uid(23)])]
            with self.subTest(kind=kind), self.assertRaises(Refused):
                assert_read_response(self.base['read_requests'][kind], bad, self.context, self.graph)
        bad = deepcopy(self.base['read_responses']['history'])
        bad['result']['items'][0]['data'] = {'raw': 'unfiltered graph history'}
        with self.assertRaises(ValidationError):
            validate('ReadResponse', bad)

    def test_empty_partial_search_page_and_context_budgets_are_explicit(self):
        response = deepcopy(self.base['read_responses']['search'])
        response['result'].update(nodes=[], next_cursor='opaque-scope-bound-token',
                                  coverage={'status': 'partial', 'complete_for': 'authorized_projection', 'reasons': ['scan_budget']})
        assert_read_response(self.base['read_requests']['search'], response, self.context, self.graph)
        request = deepcopy(self.base['read_requests']['context'])
        request['query']['max_nodes'] = 1
        result = project_context(self.graph['context'], request, self.context, self.graph)
        self.assertEqual(result['nodes'], [])
        self.assertEqual(result['coverage']['status'], 'partial')
        request['query']['max_bytes'] = 1024
        large = deepcopy(self.graph['context']);large['node']['body'] = 'x' * 2048
        self.refused('response_too_large', project_context, large, request, self.context, self.graph)

    def test_revision_missing_relationships_never_substitutes_current_links(self):
        response = deepcopy(self.base['read_responses']['revision'])
        response['result'].update(relationships_available=False, links=None)
        assert_read_response(self.base['read_requests']['revision'], response, self.context, self.graph)
        response['result']['links'] = []
        with self.assertRaises(ValidationError):
            validate('ReadResponse', response)
        response = deepcopy(self.base['read_responses']['revision'])
        response['result']['node']['content_revision'] = '2'
        self.refused('response_binding_mismatch', assert_read_response, self.base['read_requests']['revision'], response, self.context, self.graph)

    def test_workspace_boundaries_apply_even_if_record_id_is_listed(self):
        self.graph['nodes'][uid(2)]['workspace_id'] = uid(999)
        self.assertFalse(readable(uid(2), self.context, self.graph))
        self.refused('not_found', authorize_read, self.agent, self.context, self.base['read_requests']['get'], self.graph)
        raw = deepcopy(self.graph['context']);raw['nodes'][0]['workspace_id'] = uid(999)
        self.refused('upstream_binding_mismatch', project_context, raw, self.base['read_requests']['context'], self.context, GRAPH)

    def test_all_five_commands_map_to_qualified_graph_shapes(self):
        for kind, request in self.base['command_requests'].items():
            with self.subTest(kind=kind):
                context = deepcopy(self.context)
                if kind == 'propose_decomposition':
                    context['profile']['write_scope']['relationship_types'] += ['blocks', 'parent-child']
                authorize_batch(self.agent, context, request, self.graph)
                intent = translate_fixture(request, context)
                upstream_validate('DraftRequest' if kind == 'propose_decomposition' else 'MutationRequest', intent['payload'])
                self.assertEqual(intent['workspace_id'], self.context['binding']['workspace_id'])
                self.assertLessEqual(len(intent['payload']['idempotency_key'].encode()), 128)
                self.assertNotIn('token', canonical(intent).decode())

    def test_batch_scope_failure_precedes_admission_and_has_no_partial_effect(self):
        ledger = OperationLedger()
        request = deepcopy(self.base['command_requests']['create_finding'])
        invalid = deepcopy(self.base['command_requests']['add_relationship']['commands'][0]);invalid['target'] = uid(4)
        request['commands'].append(invalid)
        self.refused('not_found', ledger.admit, self.agent, self.context, request, self.graph)
        self.assertEqual(ledger.operations, {})
        self.assertNotIn(uid(10), self.graph['nodes'])

    def test_new_in_batch_nodes_need_explicit_creation_budget_and_order(self):
        request = deepcopy(self.base['command_requests']['create_finding'])
        relation = deepcopy(self.base['command_requests']['add_relationship']['commands'][0])
        relation.update(source=uid(10), expected_source_version='1')
        request['commands'].append(relation)
        authorize_batch(self.agent, self.context, request, self.graph)
        request['commands'].reverse()
        self.refused('not_found', authorize_batch, self.agent, self.context, request, self.graph)
        request = deepcopy(self.base['command_requests']['create_finding'])
        self.context['profile']['write_scope']['max_created_records'] = 0
        self.refused('scope_denied', authorize_batch, self.agent, self.context, request, self.graph)

    def test_source_version_guards_share_atomic_graph_batch_and_do_not_escalate(self):
        for kind in ['add_relationship', 'remove_relationship']:
            intent = translate_fixture(self.base['command_requests'][kind], self.context)
            self.assertEqual(intent['payload']['operations'][0], {'op': 'update_node', 'id': uid(2), 'expected_version': 2, 'patch': {}})
        request = deepcopy(self.base['command_requests']['add_relationship'])
        for node_id in [uid(5), uid(6)]:
            with self.subTest(source=node_id):
                context = deepcopy(self.context)
                context['profile']['read_scope']['record_ids'].append(node_id)
                context['profile']['write_scope']['record_ids'].append(node_id)
                request['commands'][0]['source'] = node_id
                self.refused('guard_unavailable', authorize_batch, self.agent, context, request, self.graph)

    def test_remove_relationship_uses_stored_endpoints_not_caller_claims(self):
        request = deepcopy(self.base['command_requests']['remove_relationship'])
        request['commands'][0]['target'] = uid(3)
        self.refused('not_found', authorize_batch, self.agent, self.context, request, self.graph)
        request = deepcopy(self.base['command_requests']['remove_relationship'])
        request['commands'][0].update(id=uid(22), source=uid(1), target=uid(3))
        self.refused('scope_denied', authorize_batch, self.agent, self.context, request, self.graph)

    def test_task_correction_needs_separate_role_capability(self):
        request = deepcopy(self.base['command_requests']['correct_record']);request['commands'][0]['id'] = uid(1)
        self.refused('capability_denied', authorize_batch, self.agent, self.context, request, self.graph)
        self.context['profile']['capabilities'].append('write.task_content')
        request['commands'][0]['expected_version'] = str(self.graph['nodes'][uid(1)]['version'])
        authorize_batch(self.agent, self.context, request, self.graph)
        self.graph['nodes'][uid(1)]['verification'] = 'verified'
        self.refused('guard_unavailable', authorize_batch, self.agent, self.context, request, self.graph)

    def test_proposal_never_applies_or_mixes_with_live_writes(self):
        self.context['profile']['write_scope']['relationship_types'] += ['blocks', 'parent-child']
        request = deepcopy(self.base['command_requests']['propose_decomposition'])
        authorize_batch(self.agent, self.context, request, self.graph)
        intent = translate_fixture(request, self.context)
        self.assertEqual(intent['operation_class'], 'draft_create')
        self.assertTrue(all(op['task_state'] == 'draft' for op in intent['payload']['operations'] if op['op'] == 'create_node'))
        request['commands'].append(self.base['command_requests']['create_finding']['commands'][0])
        with self.assertRaises(ValueError):
            validate('CommandRequest', request)
        request = deepcopy(self.base['command_requests']['propose_decomposition']);request['commands'][0]['guards'] = []
        self.refused('missing_precondition', authorize_batch, self.agent, self.context, request, self.graph)
        request = deepcopy(self.base['command_requests']['propose_decomposition']);request['commands'][0]['relationships'][0]['target'] = uid(4)
        self.refused('not_found', authorize_batch, self.agent, self.context, request, self.graph)

    def test_lost_graph_response_replays_exact_frozen_intent_with_one_event(self):
        request = self.base['command_requests']['correct_record']
        ledger = OperationLedger()
        pending = ledger.admit(self.agent, self.context, request, self.graph)
        self.assertEqual(pending['state'], 'pending')
        service = GraphServiceFixture(self.base['operation_statuses']['confirmed']['receipt'])
        intent = deepcopy(ledger.operations[request['operation_id']]['translated'])
        with self.assertRaises(ConnectionError):
            service.dispatch(intent, lose_response=True)
        unknown = ledger.unknown(request['operation_id'])
        self.assertEqual(unknown['effect'], 'possible')
        self.agent['active'] = False
        statuses = ledger.recover(self.base['node_principal'], self.base['node_recovery_request'])
        self.assertEqual(statuses[0], unknown)
        receipt = service.dispatch(intent)
        confirmed = ledger.confirm(request['operation_id'], receipt)
        self.assertEqual(confirmed['state'], 'confirmed')
        self.assertEqual(ledger.confirm(request['operation_id'], receipt), confirmed)
        self.assertEqual(service.logical_commits, 1)
        self.assertEqual(ledger.confirmed_events, [request['operation_id']])
        self.assertEqual(ledger.operations[request['operation_id']]['translated'], intent)
        self.refused('session_expired_or_revoked', ledger.admit, self.agent, self.context, request, self.graph)

    def test_idempotency_conflict_and_terminal_version_conflict_never_blindly_overwrite(self):
        request = deepcopy(self.base['command_requests']['correct_record']);ledger = OperationLedger()
        ledger.admit(self.agent, self.context, request, self.graph)
        changed = deepcopy(request);changed['commands'][0]['expected_version'] = '3'
        self.refused('id_conflict', ledger.admit, self.agent, self.context, changed, self.graph)
        service = GraphServiceFixture(self.base['operation_statuses']['confirmed']['receipt'])
        self.refused('version_conflict', service.dispatch, ledger.operations[request['operation_id']]['translated'], False, True)
        conflict = ledger.conflict(request['operation_id'], {'kind': 'version', 'subject_id': uid(2), 'expected': '2', 'current': '3'})
        self.assertEqual(retry_action(conflict, 1, 0, self.context['profile'], True), 'stop_and_review')
        self.refused('terminal_operation', ledger.confirm, request['operation_id'], self.base['operation_statuses']['confirmed']['receipt'])
        self.assertEqual(service.logical_commits, 0)
        self.assertEqual(ledger.confirmed_events, [])

    def test_recovery_never_creates_intents_or_crosses_node_attempt_or_generation(self):
        ledger = OperationLedger();request = self.base['command_requests']['correct_record']
        ledger.admit(self.agent, self.context, request, self.graph)
        recovery = deepcopy(self.base['node_recovery_request'])
        self.refused('unauthorized', ledger.recover, self.agent, recovery)
        for field in ['attempt_id', 'session_id', 'generation']:
            bad = deepcopy(recovery);bad['attempt'][field] = 2 if field == 'generation' else uid(999)
            self.refused('operation_unknown', ledger.recover, self.base['node_principal'], bad)
        bad = deepcopy(recovery);bad['node_id'] = uid(999)
        self.refused('unauthorized', ledger.recover, self.base['node_principal'], bad)
        bad = deepcopy(recovery);bad['operation_ids'] = [uid(999)]
        self.refused('operation_unknown', ledger.recover, self.base['node_principal'], bad)
        self.assertEqual(len(ledger.operations), 1)
        bad = deepcopy(recovery);bad['commands'] = request['commands']
        with self.assertRaises(ValidationError):
            ledger.recover(self.base['node_principal'], bad)
        self.assertEqual(len(ledger.recover(self.base['node_principal'], self.base['node_list_request'])), 1)

    def test_existing_operation_replay_does_not_bypass_attempt_binding(self):
        ledger = OperationLedger();request = self.base['command_requests']['correct_record']
        ledger.admit(self.agent, self.context, request, self.graph)
        other = deepcopy(self.context);other['binding']['attempt_id'] = uid(999)
        principal = deepcopy(self.agent);principal['binding'] = deepcopy(other['binding'])
        self.refused('stale_binding', ledger.admit, principal, other, request, self.graph)

    def test_response_receipt_scope_and_disposition_cannot_be_forged(self):
        request = self.base['command_requests']['correct_record'];receipt = deepcopy(self.base['operation_statuses']['confirmed']['receipt'])
        assert_receipt(request, receipt)
        receipt['versions'][0]['id'] = uid(4)
        self.refused('receipt_scope_denied', assert_receipt, request, receipt)
        status = deepcopy(self.base['operation_statuses']['pending']);status['state'] = 'confirmed'
        with self.assertRaises(ValidationError):validate('OperationStatus', status)
        status = deepcopy(self.base['operation_statuses']['confirmed']);status['receipt']['task_completion'] = True
        with self.assertRaises(ValidationError):validate('OperationStatus', status)
        status = deepcopy(self.base['operation_statuses']['confirmed']);status['receipt']['workspace_id'] = uid(999)
        with self.assertRaises(ValueError):validate('OperationStatus', status)

    def test_unknown_results_exhaust_retry_budget_without_fresh_keys_or_new_authority(self):
        status = self.base['operation_statuses']['unknown'];profile = self.context['profile']
        self.assertEqual(retry_action(status, 1, 0, profile, True), 'query_then_replay_identical')
        self.assertEqual(retry_action(status, 1, 0, profile, False), 'node_lookup_existing_only')
        self.assertEqual(retry_action(status, 5, 0, profile, True), 'manual_reconciliation')
        self.assertEqual(retry_action(status, 1, 30000, profile, True), 'manual_reconciliation')
        bad = deepcopy(status);bad['recovery']['remaining_attempts'] = 0
        with self.assertRaises(ValueError):validate('OperationStatus', bad)
        bad['recovery'].update(action='manual_reconciliation', after_ms=0)
        validate('OperationStatus', bad)

    def test_developer_validation_cli_never_echoes_private_invalid_payload(self):
        result = subprocess.run([sys.executable, str(ROOT / 'validate.py'), 'CommandRequest', '-'],
                                input='{"secret_token":"DO-NOT-ECHO"}', capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn('DO-NOT-ECHO', result.stdout + result.stderr)


    def test_messaging_extension_shapes_preserve_old_profile_bytes(self):
        for fixture in json.loads((ROOT / 'fixtures/messaging.json').read_text()):
            validate(fixture['type'], fixture['value'])
        profile = deepcopy(self.context['profile'])
        for cap in ['message.send', 'message.read', 'message.ack']:
            profile['capabilities'].append(cap)
        validate('ResolvedProfile', profile)
        invalid = json.loads((ROOT / 'fixtures/messaging.json').read_text())[0]['value']
        invalid['sender'] = {'kind': 'owner'}
        with self.assertRaises(ValidationError):
            validate('MessageSend', invalid)

    def test_supervisor_recipient_and_sender_are_strict_additive_branches(self):
        fixtures = {f['name']: f['value'] for f in json.loads((ROOT / 'fixtures/messaging.json').read_text())}
        send = fixtures['agent-to-supervisor-send']
        message = fixtures['supervisor-sent-session-message']
        validate('MessageSend', send)
        validate('MailboxMessage', message)
        for extra in [{'task_id': send['message_id']}, {'workspace_id': send['message_id']}]:
            bad = deepcopy(send)
            bad['recipient'].update(extra)
            with self.assertRaises(ValidationError):
                validate('MessageSend', bad)
        for change in [{'binding': {}}, {'generation': 0}, {'user_id': message['message_id']}]:
            bad = deepcopy(message)
            bad['sender'].update(change)
            with self.assertRaises(ValidationError):
                validate('MailboxMessage', bad)
        bad = deepcopy(message)
        del bad['sender']['conversation_id']
        with self.assertRaises(ValidationError):
            validate('MailboxMessage', bad)


    def test_mutation_delivery_guards_and_explicit_structural_scope(self):
        vectors = json.loads((ROOT / 'fixtures/mutation-delivery.json').read_text())
        for vector in vectors['wire']:
            validate(vector['type'], vector['value'])
        for vector in vectors['proposal_profiles']:
            context = deepcopy(self.context)
            context['profile'] = deepcopy(vector['profile'])
            request = deepcopy(self.base['command_requests']['propose_decomposition'])
            if vector['authorized']:
                authorize_batch(self.agent, context, request, self.graph)
            else:
                self.refused('scope_denied', authorize_batch, self.agent, context, request, self.graph)
        for kind, field in [('correct_record', 'expected_version'), ('add_relationship', 'expected_source_version')]:
            request = deepcopy(self.base['command_requests'][kind])
            request['commands'][0][field] = '3'
            self.refused('version_conflict', authorize_batch, self.agent, self.context, request, self.graph)


if __name__ == '__main__':
    unittest.main()
