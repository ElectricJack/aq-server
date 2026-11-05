"""Reconstruct the legacy registry tuple from base 86ca279's serializer.

Input matches credential_legacy_default_config_digest_is_unchanged at c4afb9ce.
This independent check complements, rather than replaces, the Rust regression.
"""

import hashlib
import json

credential = {
    "profile": {"id": "test", "version": "1", "digest": "a" * 64},
    "provider": "test",
    "account_reference": "acct",
    "endpoint_reference": "endpoint",
    "revision": 3,
    "enabled": True,
    "provisionable": False,
}
placement = [credential, ["00000000-0000-4000-8000-000000000001"]]
registry = [7, [placement], [], [credential], [None]]
canonical = json.dumps(registry, sort_keys=True, separators=(",", ":")).encode()
actual = hashlib.sha256(canonical).hexdigest()
assert actual == "35c9911673be2ce9dece9de54ea211f45be4b59f83f6b238e08a215cd0873ebd"
print(actual)
