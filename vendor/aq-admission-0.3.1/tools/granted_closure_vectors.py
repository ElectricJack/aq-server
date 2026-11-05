#!/usr/bin/env python3
"""Independent standard-library canonical vectors; no authorization/proof claim."""
import hashlib
import json
from pathlib import Path
root = Path(__file__).resolve().parent.parent
fixture = json.loads((root / 'artifacts/granted-closure.fixture.json').read_text())
def digest(domain, value):
    data = json.dumps([domain, value], sort_keys=True, ensure_ascii=False, separators=(',', ':')).encode()
    return hashlib.sha256(data).hexdigest()
requests = fixture['valid_requests']
unicode_request = json.loads(json.dumps(requests[0]))
unicode_request['runtime']['id'] = 'runtime/雪'
output = {
    'requests': [digest('aq.granted-launch-closure.request.v1', r) for r in requests],
    'suppression': digest('aq.launch-suppression.v1', requests[0]['evidence']['record']),
    'host_proofs': [digest('aq.granted-closure.host-proof.v1', r['evidence']['proof']) for r in requests[1:]],
    'unicode_request': digest('aq.granted-launch-closure.request.v1', unicode_request),
}
print(json.dumps(output, indent=2))
