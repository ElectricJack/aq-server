"""Wire fixtures and app-level invariants; retained provenance needs SQL tests."""
import hashlib
import json
from pathlib import Path
from uuid import UUID
import jsonschema

ROOT = Path(__file__).parent
schema = json.loads((ROOT / 'schema.json').read_text())
format_checker = jsonschema.FormatChecker()

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()

def validate(kind, value):
    document = dict(schema, **{'$ref': '#/$defs/' + kind})
    document.pop('anyOf', None)
    jsonschema.Draft202012Validator(document, format_checker=format_checker).validate(value)
    if 'operation_id' in value:
        assert UUID(value['operation_id']).int
        assert 0 < int(value['expected_version']) <= 2**63 - 1
    if kind in ('Comment', 'Correction'):
        assert value['body'].strip() and 1 <= len(value['body'].encode()) <= 16384
    if kind == 'Comment':
        a = value['anchor']
        if a['kind'] == 'image_region':
            assert a['width'] > 0 and a['height'] > 0
            assert a['x'] + a['width'] <= 1 and a['y'] + a['height'] <= 1
        if a['kind'] == 'video_range':
            assert 0 <= a['start_ms'] < a['end_ms'] <= 60000
    if kind == 'FeedbackDraft':
        assert value['title'].strip() and 1 <= len(value['title']) <= 200
        assert not any(ord(c) < 32 for c in value['title'])
        assert 1 <= len(value['description'].encode()) <= 32768
        assert 1 <= len(value['acceptance_criteria'].encode()) <= 8192
        assert int(value['expected_project_revision']) > 0
    assert len(canonical(value)) <= 65536

for fixture in json.loads((ROOT / 'fixtures/valid.json').read_text()):
    validate(fixture['type'], fixture['value'])
for fixture in json.loads((ROOT / 'fixtures/invalid.json').read_text()):
    try:
        validate(fixture['type'], fixture['value'])
    except (AssertionError, ValueError, KeyError, jsonschema.ValidationError):
        continue
    raise AssertionError('invalid fixture accepted: ' + fixture['name'])
replay = json.loads((ROOT / 'fixtures/replay.json').read_text())
for key in ('original', 'identical', 'changed'):
    validate('Comment', replay[key])
assert canonical(replay['original']) == canonical(replay['identical'])
assert canonical(replay['original']) != canonical(replay['changed'])
assert replay['original']['operation_id'] == replay['changed']['operation_id']
assert replay['identical_disposition'] == 'confirmed' and replay['changed_status'] == 409
assert replay['canonical_sha256'] == hashlib.sha256(canonical(replay['original'])).hexdigest()
compat = json.loads((ROOT / 'fixtures/compatibility.json').read_text())
read = json.loads((ROOT / '../../dashboard-read/v1/schema.json').resolve().read_text())
actions = json.loads((ROOT / '../../dashboard-actions/v1/schema.json').resolve().read_text())
jsonschema.validate(compat['read'], read)
jsonschema.validate(compat['action'], actions)
for contract in compat['read']['data']['contracts']:
    contract_schema = ROOT / '../../' / contract['name'] / 'v1/schema.json'
    assert contract['digest'] == 'sha256:' + hashlib.sha256(contract_schema.read_bytes()).hexdigest()
assert compat['read']['api_version'] == 'dashboard-read-1'
assert compat['action']['kind'] == 'result.comment'
print('Results feedback: valid, invalid, replay and compatibility fixtures passed')

# Pin the original strict schema. Legacy clients must ignore the documented
# additive fields; clients that reject unknown fields need a decoder update.
legacy_bytes = (ROOT / 'schema-1.0.json').read_bytes()
legacy_schema = json.loads(legacy_bytes)
additions = compat['additive_reads']
assert hashlib.sha256(legacy_bytes).hexdigest() == additions['legacy_schema_sha256']

def validate_legacy(kind, value):
    document = dict(legacy_schema, **{'$ref': '#/$defs/' + kind})
    document.pop('anyOf', None)
    jsonschema.Draft202012Validator(document, format_checker=format_checker).validate(value)

for kind, old_key, new_key in (
    ('Detail', 'legacy_detail', 'detail_1_1'),
    ('CommentsPage', 'legacy_comments', 'comments_1_1'),
):
    validate(kind, additions[old_key])  # additive decoder accepts old wire data
    validate_legacy(kind, additions[old_key])
    validate(kind, additions[new_key])
    projected = json.loads(json.dumps(additions[new_key]))
    projected.pop('feedback_read_version')
    assert projected.pop('follow_up_submission') == {'state': 'unsupported'}
    if kind == 'Detail':
        assert projected['result'].pop('directly_following') is False
        # This existing array now contains retained exact resolution thread IDs.
        assert projected['revision']['addressed_feedback']
        projected['revision']['addressed_feedback'] = []
    else:
        for comment in projected['items']:
            assert comment.pop('feedback_links')[0]['origin']['comment_revision'] == '1'
            assert comment.pop('resolution')['follow_up']['revision'] == 'a' * 64
    assert projected == additions[old_key]
    validate_legacy(kind, projected)

reload = replay['read_reload']
for response in reload.values():
    validate('CommentsPage', response)
assert reload['before'] == reload['after_identical_replay'] == reload['after_reload']
print('Results feedback 1.1: additive reads, exact origins and reload fixtures passed')
