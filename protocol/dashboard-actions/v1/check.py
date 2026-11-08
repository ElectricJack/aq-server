#!/usr/bin/env python3
"""Check the server-owned union, retained examples and replay relationships."""
import copy
import hashlib
import json
from pathlib import Path
from uuid import UUID
import jsonschema

root = Path(__file__).resolve().parent
schema = json.loads((root / 'schema.json').read_text())
read_schema = json.loads((root.parents[1] / 'dashboard-read/v1/schema.json').read_text())
jsonschema.Draft202012Validator.check_schema(schema)
validator = jsonschema.Draft202012Validator(schema, format_checker=jsonschema.FormatChecker())
read_validator = jsonschema.Draft202012Validator(read_schema)
def load(name):
    return json.loads((root / 'fixtures' / (name + '.json')).read_text())
def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()
TYPED = ('task.graph.create', 'task.edit', 'task.cancel', 'task.retry', 'task.reprioritize', 'task.dependency.add',
         'task.dependency.remove', 'knowledge.author', 'knowledge.correct', 'knowledge.verify',
         'knowledge.retire', 'knowledge.restore')
def validate_task(task):
    assert task['title'].strip() and len(task['title']) <= 512
    assert not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in task['title'])
    assert len(task.get('body', '').encode()) <= 32768 and len(task.get('acceptance_criteria', '').encode()) <= 8192
    labels = task.get('labels', [])
    assert len(labels) <= 16 and len(set(labels)) == len(labels)
    assert all(s.strip() and len(s.encode()) <= 128 for s in labels)
    if task.get('priority') is not None:
        assert 1 <= task['priority'] <= 300
    for field in ('profile_hint', 'intelligence_hint', 'task_type'):
        if task.get(field) is not None:
            assert task[field].strip() and len(task[field].encode()) <= 128
            assert not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in task[field])
PLAYBOOK = ('playbook.run.start', 'playbook.run.cancel', 'playbook.activation.activate', 'playbook.activation.rollback')
def validate_playbook(value):
    payload, subject, kind = value['payload'], value['subject'], value['kind']
    assert UUID(payload['project_id']).int
    if kind == 'playbook.run.cancel':
        assert subject == {'type':'playbook_run', 'id':payload['run_id']} and value['expected_versions'] == {}
        assert 'expected_version' not in payload
    else:
        revision = payload['expected_version']
        assert subject == {'type':'playbook_binding', 'id':payload['binding'], 'revision':revision}
        assert value['expected_versions'] == {'playbook_binding':revision} and 0 < int(revision) <= 2**63 - 1
        assert payload['artifact'].startswith('sha256:') and len(payload['artifact']) == 71
        if kind == 'playbook.run.start':
            assert payload['entry'].strip() and len(payload.get('inputs', {})) <= 32
            assert all(isinstance(v, (bool, int, float, str)) for v in payload.get('inputs', {}).values())
    assert len(canonical(value)) <= 65536
WORKER = ('worker.start', 'worker.stop', 'worker.drain', 'worker.revoke')
COMMENT = ('task.comment.add', 'task.comment.edit', 'task.comment.delete')
def validate_comment(value):
    payload, subject, kind = value['payload'], value['subject'], value['kind']
    assert UUID(payload['project_id']).int and UUID(subject['id']).int
    if kind == 'task.comment.add':
        assert subject == {'type':'task', 'id':subject['id']} and value['expected_versions'] == {}
        assert 'expected_revision' not in payload
    else:
        revision = payload['expected_revision']
        assert subject == {'type':'task_comment', 'id':subject['id'], 'revision':revision}
        assert value['expected_versions'] == {'task_comment':revision} and 0 < int(revision) <= 2**63 - 1
    if kind != 'task.comment.delete':
        body = payload['body']
        assert body.strip() and len(body.encode()) <= 16384
        assert not any(ord(c) < 32 and c not in '\t\n\r' or 127 <= ord(c) <= 159 for c in body)
    else:
        assert 'body' not in payload
    assert len(canonical(value)) <= 65536
def validate_request(value):
    validator.validate(value)
    payload, subject = value['payload'], value['subject']
    assert UUID(value['operation_id']).int and payload['operation_id'] == value['operation_id']
    kind = value['kind']
    if kind in PLAYBOOK:
        return validate_playbook(value)
    if kind in COMMENT:
        return validate_comment(value)
    if kind in WORKER:
        assert subject == {'type':'machine', 'id':payload['machine']} and value['expected_versions'] == {}
        assert ('expected_generation' in payload) == (kind != 'worker.revoke')
        return
    key = {'task.graph.create':'project', 'task.create':'project', 'decision.resolve':'decision', 'supervisor.message.send':'conversation', 'session.message.send':'session'}.get(kind)
    key = key or ('project' if kind == 'knowledge.author' else kind.split('.')[0])
    field = {'task.create':'expected_project_revision', 'decision.resolve':'expected_decision_revision', 'supervisor.message.send':'expected_conversation_revision', 'knowledge.author':'expected_project_revision', 'session.message.send':'expected_generation'}.get(kind, 'expected_version')
    revision = str(payload[field])
    assert value['expected_versions'] == {key:revision} and subject['revision'] == revision
    assert subject['type'] == key and UUID(subject['id']).int
    assert 0 < int(revision) <= 2**63 - 1
    if kind == 'task.graph.create':
        assert subject['id'] == payload['project_id'] and UUID(payload['project_id']).int
        tasks, edges = payload['tasks'], payload.get('edges', [])
        ids = {t['id'] for t in tasks}
        assert len(ids) == len(tasks) and all(UUID(i).int for i in ids)
        assert 1 <= len(tasks) + len(edges) + sum('parent_id' in t and t['parent_id'] is not None for t in tasks) <= 100
        edge_ids, pairs = set(), set()
        for task in tasks:
            validate_task(task)
            if task.get('parent_id'):
                assert UUID(task['parent_id']).int and task['parent_id'] != task['id']
        for edge in edges:
            assert UUID(edge['id']).int and edge['id'] not in ids | edge_ids
            assert UUID(edge['source']).int and UUID(edge['target']).int and edge['source'] != edge['target']
            assert edge['source'] in ids or edge['target'] in ids
            assert (edge['source'], edge['target']) not in pairs
            edge_ids.add(edge['id'])
            pairs.add((edge['source'], edge['target']))
    elif kind == 'task.create':
        validate_task(payload)
    elif kind == 'supervisor.message.send':
        assert UUID(payload['message_id']).int and payload['message_id'] != value['operation_id']
        assert payload['body'].strip() and len(payload['body'].encode()) <= 16384
        assert payload['context'] == [] and payload['attachments'] == []
    elif kind == 'session.message.send':
        assert UUID(payload['project_id']).int and UUID(payload['message_id']).int
        assert payload['message_id'] != value['operation_id']
        assert payload['body'].strip() and len(payload['body'].encode()) <= 16384
    elif kind in TYPED:
        assert UUID(payload['project_id']).int
        if kind == 'knowledge.author':
            assert subject['id'] == payload['project_id'] and payload['title'].strip() and payload['body'].strip()
        if kind in ('task.cancel', 'knowledge.verify'):
            assert value['reason'].strip()
        if kind == 'task.reprioritize':
            assert -1_000_000 <= payload['priority'] <= 1_000_000
    else:
        assert payload['decision_id'] == subject['id'] and payload['schema_version'] == 1
        assert payload['expected_subject']['version'] == '1'
    assert len(canonical(value)) <= 65536

for name in ('task-create', 'decision-resolve', 'supervisor-message-send', 'session-message-send') + tuple(k.replace('.', '-') for k in TYPED + PLAYBOOK + COMMENT + WORKER):
    validate_request(load('request-' + name))
conflict = load('operation-conflict-current-versions')
validator.validate(conflict)
operation = conflict['data']
assert operation['disposition'] == 'conflict' and operation['recovery'] == 'refresh_and_review'
assert operation['current_versions'] == {'knowledge':'3'} and conflict['pending_operations'] == []
assert not operation['receipt_refs'] and not operation['affected_entities']
assert operation['request_digest'] == 'sha256:' + hashlib.sha256(canonical({'request':load('request-knowledge-correct'), 'scope':operation['scope']})).hexdigest()
comment_conflict = load('operation-task-comment-conflict')
validator.validate(comment_conflict)
assert comment_conflict['data']['current_versions'] == {'task_comment':'2'}
assert comment_conflict['data']['subject_refs'][0]['type'] == 'task_comment'
for disposition in ('pending', 'confirmed', 'conflict', 'rejected', 'unknown'):
    value = load('operation-' + disposition)
    validator.validate(value)
    assert value['api_version'] == 'dashboard-actions-1'
    operation = value['data']
    assert operation['disposition'] == disposition
    unresolved = disposition in ('pending', 'unknown')
    assert value['pending_operations'] == ([operation['operation_id']] if unresolved else [])
    assert operation['recovery'] == ('lookup_original' if unresolved else 'none' if disposition == 'confirmed' else 'refresh_and_review')
    if disposition != 'confirmed':
        assert not operation['receipt_refs'] and not operation['affected_entities']
    request = load('request-supervisor-message-send')
    assert operation['request_digest'] == 'sha256:' + hashlib.sha256(canonical({'request':request, 'scope':operation['scope']})).hexdigest()
for status in (401,403,404,409,410,413,422,426,429,503):
    error = load('error-' + str(status))
    validator.validate(error)
    assert error['retryable'] == (status in (429,503))
    if status == 404:
        assert 'operation_id' not in error
validator.validate(load('expired-cursor'))
assert load('expired-cursor')['code'] == 'cursor_restart_required'
for fixture in ('replay', 'replay-task-graph-create'):
    replay = load(fixture)
    for key in ('original','identical','changed'):
        validate_request(replay[key])
    assert canonical(replay['original']) == canonical(replay['identical'])
    assert canonical(replay['original']) != canonical(replay['changed'])
    assert all(replay[key]['operation_id'] == replay['original_operation_id'] for key in ('original','identical','changed'))
    assert replay['identical_disposition'] == 'confirmed' and replay['changed_status'] == 409
for case in load('invalid'):
    try:
        validate_request(case['request'])
    except (AssertionError, jsonschema.ValidationError, ValueError, KeyError):
        continue
    raise AssertionError('invalid request accepted: ' + case['name'])
compat = load('compatibility')
read_validator.validate(compat['read'])
validator.validate(compat['action'])
assert not read_validator.is_valid(compat['read_with_action_version'])
assert not validator.is_valid(compat['action_with_read_version'])
for contract in compat['read']['data']['contracts']:
    path = root.parents[1] / contract['name'] / 'v1/schema.json'
    assert contract['digest'] == 'sha256:' + hashlib.sha256(path.read_bytes()).hexdigest()
assert len(compat['read']['data']['contracts']) == 2
assert compat['read']['api_version'] == 'dashboard-read-1'
validate_request(load('request-task-create-metadata'))
print('24 requests, 5 dispositions, current-versions conflicts, 10 errors, cursor, replay, invalid and compatibility fixtures passed')
