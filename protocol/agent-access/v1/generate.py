#!/usr/bin/env python3
"""Generate public JSON Schema, OpenAPI and development type bindings; no gateway."""
from pathlib import Path
import argparse
import json

ROOT = Path(__file__).resolve().parent
SCHEMA_ID = 'urn:agent-q:agent-access:1'
DEFS = {}


def ref(name):
    return {'$ref': '#/$defs/' + name}


def obj(name, fields, optional=(), **extra):
    DEFS[name] = {'type': 'object', 'additionalProperties': False, 'properties': fields,
                  'required': [k for k in fields if k not in optional], **extra}
    return ref(name)


def enum(name, values):
    DEFS[name] = {'type': 'string', 'enum': values}
    return ref(name)


def union(name, values):
    DEFS[name] = {'oneOf': [ref(x) for x in values]}
    return ref(name)


def array(item, maximum=64, minimum=0, unique=False):
    return {'type': 'array', 'items': item, 'minItems': minimum, 'maxItems': maximum,
            **({'uniqueItems': True} if unique else {})}


def text(maximum, minimum=0):
    return {'type': 'string', 'minLength': minimum, 'maxLength': maximum, 'x-utf8-maxBytes': maximum}


def integer(maximum, minimum=0):
    return {'type': 'integer', 'minimum': minimum, 'maximum': maximum}


def nullable(value):
    return {'anyOf': [value, {'type': 'null'}]}


def const(value):
    return {'const': value}


UUID = {'type': 'string', 'format': 'uuid', 'pattern': '^[0-9a-f-]{36}$',
        'not': {'const': '00000000-0000-0000-0000-000000000000'}}
SHA = {'type': 'string', 'pattern': '^[0-9a-f]{64}$'}
VERSION = {'type': 'string', 'pattern': '^[1-9][0-9]{0,18}$', 'x-int64-token': True}
TIME = {'type': 'string', 'format': 'date-time'}
BOOL = {'type': 'boolean'}
API = const(1)
EDGE_TYPES = ['blocks', 'parent-child', 'references', 'motivated_by', 'produces', 'supports', 'contradicts', 'supersedes']
INFO_TYPES = EDGE_TYPES[2:]
CAPABILITIES = ['read.get', 'read.revision', 'read.history', 'read.search', 'read.context', 'read.relationships',
                'write.finding', 'write.correct', 'write.proposal', 'write.relationships', 'write.task_content', 'message.send', 'message.read', 'message.ack']


def definitions():
    enum('EdgeType', EDGE_TYPES)
    enum('InformationalEdgeType', INFO_TYPES)
    enum('Capability', CAPABILITIES)

    obj('TaskMailbox', {'kind': const('task'), 'workspace_id': UUID, 'project_id': UUID, 'task_id': UUID})
    obj('SessionMailbox', {'kind': const('session'), 'workspace_id': UUID, 'project_id': UUID, 'task_id': UUID,
                           'session_id': UUID, 'attempt_id': UUID, 'generation': integer(2**63-1, 1)})
    obj('SupervisorMailbox', {'kind': const('global_supervisor'), 'conversation_id': UUID})
    union('MailboxRecipient', ['TaskMailbox', 'SessionMailbox', 'SupervisorMailbox'])
    obj('MessageSend', {'operation_id': UUID, 'message_id': UUID, 'thread_id': UUID,
                        'recipient': ref('MailboxRecipient'), 'body': text(16384, 1)})
    cursor = {'type': 'string', 'pattern': '^(0|[1-9][0-9]{0,18})$'}
    obj('MessageInbox', {'recipient': ref('MailboxRecipient'), 'after': cursor, 'thread_id': nullable(UUID),
                         'limit': integer(32, 1)}, optional=('thread_id',))
    obj('MessageAck', {'operation_id': UUID, 'recipient': ref('MailboxRecipient'), 'message_id': UUID,
                       'disposition': {'enum': ['received', 'read']}})

    delivery = const('not_attempted')
    obj('MessageSendReceipt', {'api_version': API, 'operation_id': UUID, 'message_id': UUID,
                               'recipient': ref('MailboxRecipient'), 'thread_id': UUID, 'cursor': VERSION,
                               'disposition': const('queued'), 'node_delivery': delivery,
                               'harness_delivery': delivery, 'notification_delivery': delivery})
    obj('MessageAckReceipt', {'api_version': API, 'operation_id': UUID, 'message_id': UUID,
                              'recipient': ref('MailboxRecipient'), 'cursor': VERSION, 'attempt_id': UUID,
                              'disposition': {'enum': ['received', 'read']}, 'node_delivery': delivery,
                              'harness_delivery': delivery})
    obj('OwnerMessageSender', {'kind': const('owner'), 'user_id': UUID})
    obj('SessionMessageSender', {'kind': const('session'), 'binding': ref('Binding')})
    obj('SupervisorMessageSender', {'kind': const('central_supervisor'), 'actor_id': UUID,
                                    'generation': integer(2**63-1, 1), 'session_id': UUID, 'conversation_id': UUID})
    union('MessageSender', ['OwnerMessageSender', 'SessionMessageSender', 'SupervisorMessageSender'])
    obj('MailboxMessage', {'message_id': UUID, 'thread_id': UUID, 'cursor': VERSION,
                           'sender': ref('MessageSender'), 'recipient': ref('MailboxRecipient'),
                           'body': text(16384, 1), 'payload_digest': SHA, 'created_at_ms': integer(2**63-1),
                           'disposition': {'enum': ['queued', 'received', 'read']},
                           'delivery': nullable(ref('MessageDelivery'))})
    node_states = ['pending', 'received_by_node', 'handed_to_harness', 'delivery_unknown', 'stale']
    obj('MessageDelivery', {'attempt_id': UUID, 'disposition': {'enum': node_states}})
    obj('OmittedMailboxMessage', {'message_id': UUID, 'cursor': VERSION, 'body_omitted': const(True),
                                 'body_bytes': integer(16384, 1), 'recovery': const('message_body'), 'body_request': ref('MessageBodyRequest')})
    union('MailboxEntry', ['MailboxMessage', 'OmittedMailboxMessage'])
    obj('MessageInboxResponse', {'api_version': API, 'recipient': ref('MailboxRecipient'), 'after': cursor,
                                 'next_cursor': cursor, 'has_more': BOOL, 'messages': array(ref('MailboxEntry'), 32)})
    obj('MessageBodyRequest', {'recipient': ref('MailboxRecipient'), 'message_id': UUID,
                              'offset': cursor, 'max_bytes': integer(16384, 1)})
    obj('MessageBodyResponse', {'api_version': API, 'message_id': UUID, 'cursor': VERSION,
                               'payload_digest': SHA, 'offset': cursor, 'next_offset': cursor,
                               'complete': BOOL, 'body': text(16384)})
    obj('MessageNodeIdentity', {'node_id': UUID, 'boot_id': UUID})
    obj('MessageNodePoll', {'identity': ref('MessageNodeIdentity'), 'attempt': ref('AttemptRef'), 'limit': integer(16, 1)})
    obj('MessageNodeAck', {'operation_id': UUID, 'identity': ref('MessageNodeIdentity'), 'attempt': ref('AttemptRef'),
                          'delivery_id': UUID, 'disposition': {'enum': node_states[1:]}})
    obj('MessageNodeDelivery', {'delivery_id': UUID, 'message_id': UUID, 'recipient': ref('MailboxRecipient'),
                               'thread_id': UUID, 'cursor': VERSION, 'body': text(16384, 1), 'payload_digest': SHA,
                               'sender': ref('MessageSender'), 'disposition': {'enum': node_states[:2]}})
    obj('MessageNodeBatch', {'api_version': API, 'attempt': ref('AttemptRef'), 'deliveries': array(ref('MessageNodeDelivery'), 16)})
    obj('MessageNodeAckReceipt', {'api_version': API, 'operation_id': UUID, 'delivery_id': UUID,
                                 'attempt': ref('AttemptRef'), 'disposition': {'enum': node_states[1:]}})


    enum('NodeKind', ['task', 'knowledge'])
    enum('ContentType', ['fact', 'decision', 'policy', 'procedure', 'incident', 'reference', 'note'])
    enum('Verification', ['unverified', 'verified', 'disputed'])
    enum('TaskState', ['draft', 'active', 'completed', 'canceled'])
    obj('AttemptRef', {'attempt_id': UUID, 'session_id': UUID, 'generation': integer(2**64-1, 1)})
    obj('Binding', {'task_id': UUID, 'attempt_id': UUID, 'session_id': UUID, 'generation': integer(2**64-1, 1),
                    'node_id': UUID, 'boot_id': UUID, 'project_id': UUID, 'workspace_id': UUID,
                    'resolved_spec_digest': SHA, 'profile_digest': SHA, 'context_digest': SHA})
    obj('ReadScope', {'mode': {'enum': ['workspace', 'records']}, 'record_ids': array(UUID, 256, unique=True),
                      'include_created_by_attempt': BOOL, 'edge_types': array(ref('EdgeType'), 8, unique=True)},
        allOf=[{'if': {'properties': {'mode': const('workspace')}}, 'then': {'properties': {'record_ids': {'maxItems': 0}}}}])
    obj('WriteScope', {'record_ids': array(UUID, 256, unique=True), 'relationship_types': array(ref('EdgeType'), 8, unique=True),
                       'correct_fields': array({'enum': ['title', 'body', 'labels']}, 3, unique=True),
                       'max_created_records': integer(256), 'max_proposal_tasks': integer(8)})
    obj('RetryBudget', {'max_attempts': integer(8, 1), 'max_elapsed_ms': integer(120000, 1),
                        'initial_delay_ms': integer(10000, 1), 'max_delay_ms': integer(30000, 1)})
    obj('ResolvedModel', {'provider': text(128, 1), 'model': text(128, 1), 'configuration_digest': SHA})
    obj('ResolvedProfile', {'profile_id': text(128, 1), 'revision': VERSION, 'role': text(128, 1),
                            'model': ref('ResolvedModel'), 'instructions_digest': SHA, 'tool_policy_digest': SHA,
                            'capabilities': array(ref('Capability'), len(CAPABILITIES), unique=True),
                            'read_scope': ref('ReadScope'), 'write_scope': ref('WriteScope'),
                            'retry_budget': ref('RetryBudget'), 'max_response_bytes': integer(1048576, 1024)})
    obj('CredentialMetadata', {'kind': const('agent_session'), 'version': VERSION, 'expires_at': TIME,
                               'authority_revision': VERSION})
    obj('ScopedNode', {'id': UUID, 'kind': ref('NodeKind'), 'version': VERSION, 'content_revision': VERSION,
                       'title': text(512, 1), 'body': text(262144), 'acceptance_criteria': text(16384),
                       'labels': array(text(128, 1), 64, unique=True), 'task_state': nullable(ref('TaskState')),
                       'verification': nullable(ref('Verification')), 'content_type': nullable(ref('ContentType')),
                       'retired': BOOL, 'deleted': BOOL})
    obj('ScopedEdge', {'id': UUID, 'source': UUID, 'target': UUID, 'edge_type': ref('EdgeType'), 'version': VERSION,
                       'source_revision': nullable(VERSION), 'target_revision': nullable(VERSION),
                       'description': nullable(text(4096))})
    obj('Coverage', {'status': {'enum': ['complete', 'partial']}, 'complete_for': const('authorized_projection'),
                     'reasons': array({'enum': ['scan_budget', 'node_budget', 'edge_budget', 'byte_budget']}, 4, unique=True)},
        allOf=[{'if': {'properties': {'status': const('complete')}}, 'then': {'properties': {'reasons': {'maxItems': 0}}},
                'else': {'properties': {'reasons': {'minItems': 1}}}}])
    obj('ContextSnapshot', {'observed_at': TIME, 'projection': const('authorized_scope'), 'root_ids': array(UUID, 16, 1, True),
                            'nodes': array(ref('ScopedNode'), 64), 'edges': array(ref('ScopedEdge'), 256),
                            'pinned_revisions': array(ref('ScopedNode'), 64), 'coverage': ref('Coverage')})
    obj('ContextResponse', {'api_version': API, 'binding': ref('Binding'), 'credential': ref('CredentialMetadata'),
                            'profile': ref('ResolvedProfile'), 'context': ref('ContextSnapshot')})
    obj('GetQuery', {'kind': const('get'), 'node_id': UUID})
    obj('RevisionQuery', {'kind': const('revision'), 'node_id': UUID, 'revision': VERSION})
    obj('HistoryQuery', {'kind': const('history'), 'node_id': UUID, 'limit': integer(64, 1), 'cursor': nullable(text(2048, 1))})
    obj('SearchQuery', {'kind': const('search'), 'text': text(1024, 1), 'kind_filter': nullable(ref('NodeKind')),
                        'limit': integer(64, 1), 'cursor': nullable(text(2048, 1))})
    obj('ContextQuery', {'kind': const('context'), 'node_id': UUID, 'depth': integer(5),
                         'direction': {'enum': ['both', 'incoming', 'outgoing']}, 'max_nodes': integer(64, 1),
                         'max_edges': integer(256, 1), 'max_bytes': integer(1048576, 1024)})
    obj('RelationshipsQuery', {'kind': const('relationships'), 'node_id': UUID, 'direction': {'enum': ['both', 'incoming', 'outgoing']},
                               'edge_types': array(ref('EdgeType'), 8, 1, True), 'limit': integer(64, 1),
                               'cursor': nullable(text(2048, 1))})
    union('ReadQuery', ['GetQuery', 'RevisionQuery', 'HistoryQuery', 'SearchQuery', 'ContextQuery', 'RelationshipsQuery'])
    obj('ReadRequest', {'api_version': API, 'request_id': UUID, 'binding': ref('Binding'), 'query': ref('ReadQuery')})
    obj('GetResult', {'kind': const('get'), 'node': ref('ScopedNode')})
    obj('RevisionResult', {'kind': const('revision'), 'node': ref('ScopedNode'), 'links': nullable(array(ref('ScopedEdge'), 256)),
                           'relationships_available': BOOL, 'coverage': ref('Coverage')},
        allOf=[{'if': {'properties': {'relationships_available': const(False)}}, 'then': {'properties': {'links': {'type': 'null'}}},
                'else': {'properties': {'links': {'type': 'array'}}}}])
    obj('HistoryItem', {'entry_id': UUID, 'sequence': VERSION, 'category': {'enum': ['revision', 'lifecycle', 'edge']},
                        'subject_id': UUID, 'occurred_at': TIME, 'node': nullable(ref('ScopedNode')), 'edge': nullable(ref('ScopedEdge'))})
    obj('HistoryResult', {'kind': const('history'), 'items': array(ref('HistoryItem'), 64), 'next_cursor': nullable(text(2048, 1)), 'coverage': ref('Coverage')})
    obj('SearchResult', {'kind': const('search'), 'nodes': array(ref('ScopedNode'), 64), 'next_cursor': nullable(text(2048, 1)), 'coverage': ref('Coverage')})
    obj('ContextResult', {'kind': const('context'), 'node': ref('ScopedNode'), 'nodes': array(ref('ScopedNode'), 63),
                          'edges': array(ref('ScopedEdge'), 256), 'pinned_revisions': array(ref('ScopedNode'), 64), 'coverage': ref('Coverage')})
    obj('RelationshipsResult', {'kind': const('relationships'), 'edges': array(ref('ScopedEdge'), 64),
                                'next_cursor': nullable(text(2048, 1)), 'coverage': ref('Coverage')})
    union('ReadResult', ['GetResult', 'RevisionResult', 'HistoryResult', 'SearchResult', 'ContextResult', 'RelationshipsResult'])
    obj('ReadResponse', {'api_version': API, 'request_id': UUID, 'binding': ref('Binding'), 'query_digest': SHA,
                         'observed_at': TIME, 'projection': const('authorized_scope'), 'result': ref('ReadResult')})
    obj('FindingCommand', {'kind': const('create_finding'), 'id': UUID, 'title': text(512, 1), 'body': text(262144),
                           'labels': array(text(128, 1), 64, unique=True), 'content_type': ref('ContentType')})
    obj('CorrectionPatch', {'title': text(512, 1), 'body': text(262144), 'labels': array(text(128, 1), 64, unique=True)},
        optional=('title', 'body', 'labels'), minProperties=1)
    obj('CorrectionCommand', {'kind': const('correct_record'), 'id': UUID, 'expected_version': VERSION, 'patch': ref('CorrectionPatch')})
    obj('RelationshipCommand', {'kind': const('add_relationship'), 'id': UUID, 'source': UUID, 'target': UUID,
                                'expected_source_version': VERSION, 'edge_type': ref('InformationalEdgeType'),
                                'source_revision': nullable(VERSION), 'target_revision': nullable(VERSION), 'description': nullable(text(4096))})
    obj('RemoveRelationshipCommand', {'kind': const('remove_relationship'), 'id': UUID, 'source': UUID, 'target': UUID,
                                      'expected_version': VERSION, 'expected_source_version': VERSION})
    obj('ProposedTask', {'id': UUID, 'title': text(512, 1), 'body': text(262144), 'acceptance_criteria': text(16384)})
    obj('ProposedEdge', {'id': UUID, 'source': UUID, 'target': UUID, 'edge_type': {'enum': ['blocks', 'parent-child']}})
    obj('VersionGuard', {'id': UUID, 'expected_version': VERSION})
    obj('DecompositionCommand', {'kind': const('propose_decomposition'), 'name': text(200, 1), 'parent_id': UUID,
                                 'expected_parent_version': VERSION, 'tasks': array(ref('ProposedTask'), 8, 1),
                                 'relationships': array(ref('ProposedEdge'), 16), 'guards': array(ref('VersionGuard'), 16)})
    union('GraphCommand', ['FindingCommand', 'CorrectionCommand', 'RelationshipCommand', 'RemoveRelationshipCommand', 'DecompositionCommand'])
    obj('CommandRequest', {'api_version': API, 'operation_id': UUID, 'binding': ref('Binding'), 'reason': text(4096, 1),
                           'commands': array(ref('GraphCommand'), 16, 1)})
    obj('SubjectVersion', {'kind': {'enum': ['node', 'edge']}, 'id': UUID, 'version': VERSION, 'content_revision': nullable(VERSION)})
    obj('MutationReceipt', {'kind': const('graph_mutation'), 'workspace_id': UUID, 'change_set_id': UUID,
                            'versions': array(ref('SubjectVersion'), 100), 'affected_record_ids': array(UUID, 64, unique=True), 'task_completion': const(False)})
    obj('ProposalReceipt', {'kind': const('graph_draft'), 'workspace_id': UUID, 'draft_id': UUID,
                            'proposal_digest': SHA, 'applied': const(False), 'task_completion': const(False)})
    union('GraphReceipt', ['MutationReceipt', 'ProposalReceipt'])
    common = {'api_version': API, 'operation_id': UUID, 'binding': ref('Binding'), 'payload_digest': SHA}
    obj('RecoveryHint', {'action': {'enum': ['poll_existing', 'query_or_replay_identical', 'manual_reconciliation']},
                         'after_ms': integer(30000), 'remaining_attempts': integer(8), 'remaining_elapsed_ms': integer(120000)})
    obj('PendingOperation', {**common, 'state': const('pending'), 'admission': const('recorded'),
                             'effect': {'enum': ['not_dispatched', 'possible']}, 'recovery': ref('RecoveryHint')})
    obj('ConfirmedOperation', {**common, 'state': const('confirmed'), 'admission': const('recorded'),
                               'effect': const('confirmed'), 'receipt': ref('GraphReceipt')})
    obj('ConflictItem', {'kind': {'enum': ['version', 'idempotency', 'binding_changed', 'scope_revision']},
                         'subject_id': nullable(UUID), 'expected': nullable(VERSION), 'current': nullable(VERSION)})
    obj('ConflictOperation', {**common, 'state': const('conflict'), 'admission': const('recorded'), 'effect': const('not_applied'),
                              'conflicts': array(ref('ConflictItem'), 16, 1), 'retryable': const(False)})
    obj('RejectedOperation', {**common, 'state': const('rejected'), 'admission': const('recorded'), 'effect': const('not_applied'),
                              'code': {'enum': ['invalid_request', 'capability_denied', 'session_expired', 'session_revoked',
                                                'stale_generation', 'scope_denied', 'graph_rejected', 'unsupported_capability']},
                              'retryable': const(False)})
    obj('UnknownOperation', {**common, 'state': const('unknown'), 'admission': {'enum': ['recorded', 'unknown']},
                             'effect': const('possible'), 'recovery': ref('RecoveryHint')})
    union('OperationStatus', ['PendingOperation', 'ConfirmedOperation', 'ConflictOperation', 'RejectedOperation', 'UnknownOperation'])
    obj('ErrorResponse', {'api_version': API, 'request_id': UUID, 'operation_id': nullable(UUID),
                          'code': {'enum': ['invalid_request', 'unsupported_api_version', 'unsupported_capability', 'unauthorized', 'session_expired', 'session_revoked',
                                            'stale_generation', 'scope_denied', 'not_found', 'operation_unknown', 'cursor_invalid',
                                            'id_conflict', 'overloaded', 'response_too_large', 'internal']},
                          'message': text(4096, 1), 'effect': {'enum': ['not_applied', 'unknown']},
                          'retryable': BOOL, 'recovery': nullable(ref('RecoveryHint'))})
    recovery = {'api_version': API, 'request_id': UUID, 'node_id': UUID, 'attempt': ref('AttemptRef')}
    obj('RecoveryLookup', {**recovery, 'mode': const('lookup'), 'operation_ids': array(UUID, 64, 1, True)})
    obj('RecoveryList', {**recovery, 'mode': const('list'), 'limit': integer(64, 1), 'cursor': nullable(text(2048, 1))})
    union('RecoveryRequest', ['RecoveryLookup', 'RecoveryList'])
    obj('RecoveryRetry', {**recovery, 'mode': const('lookup'), 'operation_ids': array(UUID, 1, 1, True)})
    obj('RecoveryResponse', {'api_version': API, 'request_id': UUID, 'attempt': ref('AttemptRef'),
                             'operations': array(ref('OperationStatus'), 64), 'next_cursor': nullable(text(2048, 1)),
                             'coverage': ref('Coverage')})
    return DEFS


def schema():
    return {'$schema': 'https://json-schema.org/draft/2020-12/schema', '$id': SCHEMA_ID,
            'title': 'AQ scoped agent graph access v1', '$defs': definitions(),
            'oneOf': [ref(x) for x in ['ContextResponse', 'ReadRequest', 'ReadResponse', 'CommandRequest', 'OperationStatus',
                                      'RecoveryRequest', 'RecoveryResponse', 'ErrorResponse']]}


def openapi(document):
    def remap(value):
        if isinstance(value, list):
            return [remap(x) for x in value]
        if isinstance(value, dict):
            return {k: v.replace('#/$defs/', '#/components/schemas/') if k == '$ref' else remap(v) for k, v in value.items()}
        return value
    def content(name):
        return {'application/json': {'schema': {'$ref': '#/components/schemas/' + name}}}
    def route(operation, response, request=None, node=False, statuses=None):
        statuses = statuses or ['200']
        item = {'operationId': operation, 'security': [{('NodeRecoveryBearer' if node else 'AgentSessionBearer'): []}],
                'responses': {s: {'description': 'Contract response; effect is explicit.', 'content': content(response)} for s in statuses}}
        for status in ['400', '401', '403', '404', '409', '413', '426', '503']:
            if status not in item['responses']:
                item['responses'][status] = {'description': 'Structured refusal or uncertainty; never imply mutation success.', 'content': content('ErrorResponse')}
        item['parameters'] = [{'name': 'X-AQ-Agent-API-Version', 'in': 'header', 'required': False, 'schema': {'type': 'string', 'const': '1'}}]
        for response_shape in item['responses'].values():
            response_shape['headers'] = {'X-AQ-Agent-API-Version': {'schema': {'type': 'string', 'const': '1'}},
                                         'Cache-Control': {'schema': {'type': 'string', 'const': 'no-store'}}}
        if request:
            item['requestBody'] = {'required': True, 'content': content(request)}
        return item
    commands = route('agentGraphCommand', 'OperationStatus', 'CommandRequest', statuses=['200', '202', '409', '422'])
    commands['parameters'].append({'name': 'Prefer', 'in': 'header', 'required': False,
                                   'description': 'respond-async commits admission without starting dispatch; identical replay without this header may dispatch.',
                                   'schema': {'type': 'string', 'enum': ['respond-async']}})
    lookup = route('agentOperationGet', 'OperationStatus', statuses=['200', '202', '409', '422'])
    lookup['parameters'].append({'name': 'operation_id', 'in': 'path', 'required': True, 'schema': UUID})
    return {'openapi': '3.1.0', 'info': {'title': 'AQ agent graph access', 'version': '1.0.0',
             'description': 'Versioned public agent routes. See owning implementation evidence for qualified behavior; no raw graph proxy or playbook Host API.'},
            'paths': {'/v1/agent/context': {'get': route('agentContextGet', 'ContextResponse')},
                      '/v1/agent/graph/read': {'post': route('agentGraphRead', 'ReadResponse', 'ReadRequest')},
                      '/v1/agent/messages/send': {'post': route('agentMessageSend', 'MessageSendReceipt', 'MessageSend')},
                      '/v1/agent/messages/inbox': {'post': route('agentMessageInbox', 'MessageInboxResponse', 'MessageInbox')},
                      '/v1/agent/messages/ack': {'post': route('agentMessageAck', 'MessageAckReceipt', 'MessageAck')},
                      '/v1/agent/messages/body': {'post': route('agentMessageBody', 'MessageBodyResponse', 'MessageBodyRequest')},
                      '/v2/worker/messages/poll': {'post': route('workerMessagePoll', 'MessageNodeBatch', 'MessageNodePoll', node=True)},
                      '/v2/worker/messages/ack': {'post': route('workerMessageAck', 'MessageNodeAckReceipt', 'MessageNodeAck', node=True)},
                      '/v1/agent/graph/commands': {'post': commands},
                      '/v1/agent/operations/{operation_id}': {'get': lookup},
                      '/v2/worker/agent-operations/query': {'post': route('workerAgentOperationQuery', 'RecoveryResponse', 'RecoveryRequest', node=True)},
                      '/v2/worker/agent-operations/retry': {'post': route('workerAgentOperationRetry', 'RecoveryResponse', 'RecoveryRetry', node=True)}},
            'components': {'securitySchemes': {'AgentSessionBearer': {'type': 'http', 'scheme': 'bearer', 'description': 'Opaque scoped per-attempt credential; node/worker/service credentials refused.'},
                                               'NodeRecoveryBearer': {'type': 'http', 'scheme': 'bearer', 'description': 'Enrolled node credential, own existing attempt operations only; no graph request body or new admission.'}},
                           'schemas': remap(document['$defs'])}}


def python_type(value):
    if '$ref' in value:
        return value['$ref'].split('/')[-1]
    if 'const' in value:
        return 'Literal[' + repr(value['const']) + ']'
    if 'enum' in value:
        return 'Literal[' + ', '.join(repr(x) for x in value['enum']) + ']'
    variants = value.get('oneOf', value.get('anyOf'))
    if variants:
        return 'Union[' + ', '.join(python_type(x) for x in variants) + ']'
    if value.get('type') == 'array':
        return 'list[' + python_type(value.get('items', {})) + ']'
    return {'string': 'str', 'integer': 'int', 'boolean': 'bool', 'null': 'None'}.get(value.get('type'), 'Any')


def bindings(document):
    lines = ['# Generated by generate.py; development/client type declarations, no runtime SDK.',
             'from __future__ import annotations', 'from typing import Any, Literal, NotRequired, TypeAlias, TypedDict, Union', '']
    for name, value in document['$defs'].items():
        if value.get('type') != 'object':
            continue
        lines.append(f'class {name}(TypedDict):')
        for field, shape in value['properties'].items():
            annotation = python_type(shape)
            if field not in value['required']:
                annotation = f'NotRequired[{annotation}]'
            lines.append(f'    {field}: {annotation}')
        lines.append('')
    for name, value in document['$defs'].items():
        if value.get('type') != 'object':
            lines.append(f'{name}: TypeAlias = {python_type(value)}')
    return '\n'.join(lines) + '\n'


def render():
    document = schema()
    return {'schema.json': json.dumps(document, indent=2) + '\n',
            'openapi.json': json.dumps(openapi(document), indent=2) + '\n', 'client_types.py': bindings(document)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    for name, expected in render().items():
        path = ROOT / name
        if args.check:
            if not path.exists() or path.read_text() != expected:
                raise SystemExit(f'generated contract drift: {name}')
        else:
            path.write_text(expected)
    print('Generated contract files match.' if args.check else 'Generated schema, OpenAPI and development types.')


if __name__ == '__main__':
    main()
