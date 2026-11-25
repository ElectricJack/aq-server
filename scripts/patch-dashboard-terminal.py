#!/usr/bin/env python3
"""Apply a bounded local terminal repair to the verified v0.1.5 dashboard.

Keep the published archive unchanged. Rename assets whose bytes change (and
all referencing JS/CSS) to a patch-specific cache namespace, then regenerate
integrity metadata with explicitly dirty, unofficial provenance.
"""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ASSET = 'assets/InteractiveTerminal-Di3hRKTH.js'


def once(text, old, new):
    if text.count(old) != 1:
        raise ValueError('dashboard terminal patch does not match the pinned bundle')
    return text.replace(old, new)


def apply(files, root=ROOT):
    files = dict(files)
    source = root / 'vendor/dashboard-terminal-fix'
    manual = (source / 'manual-client.js').read_text()
    resolve = (source / 'resolve-attempt.js').read_text()
    text = files[ASSET].decode()
    start = text.index('function de(e,t=')
    end = text.index('function E(e){if(!e)', start)
    text = text[:start] + manual + text[end:]
    start = text.index('async function Le(e,t,r)')
    end = text.index('function Re(e,t)', start)
    text = text[:start] + resolve + text[end:]
    # No silent finite-relay fallback after discovery or connect failure.
    text = once(text, 'de(e,Ie(n),`attempt`)', 'de(e,Ie(n),e.allowControl?`attempt`:`attempt-watch`)')
    text = once(text, 'state:e=>{a=e,l()}}))});function u(t)',
                'state:e=>{a=e,l()}}))}).catch(()=>{t.signal.aborted||e.state({message:`Agent terminal unavailable — reconnect to retry`,connected:false,controlling:false,canControl:false,paused:true,outputObservedAtMs:null})});function u(t)')
    text = once(text, 'release:async()=>{await Promise.all([i?.release(),r?.release()])}',
                'release:async()=>{t.abort();await Promise.all([i?.release(),r?.release()])}')
    # Compact task panes must expose connecting/empty states too.
    text = once(text, '(!v||S||_&&(qe===`disconnected`||A.paused))',
                '(!v||S||_&&(!A.connected||!A.outputObservedAtMs||A.paused))')
    files[ASSET] = text.encode()
    namespace = hashlib.sha256(files[ASSET]).hexdigest()[:16]
    renamed = {name: name.rsplit('.', 1)[0] + '-aq' + namespace + '.' + name.rsplit('.', 1)[1]
               for name in files if name.endswith(('.js', '.css'))}
    result = {}
    for name, data in files.items():
        if name == 'dashboard-manifest.json':
            continue
        if name.endswith(('.js', '.css', '.html')):
            for old, new in renamed.items():
                data = data.replace(old.removeprefix('assets/').encode(), new.removeprefix('assets/').encode())
        result[renamed.get(name, name)] = data
    manifest = json.loads(files['dashboard-manifest.json'])
    manifest['source'].update(dirty=True, official=False)
    manifest['files'] = {name: {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}
                         for name, data in result.items()}
    result['dashboard-manifest.json'] = (json.dumps(manifest, sort_keys=True, separators=(',', ':')) + '\n').encode()
    return result
