#!/usr/bin/env node
/** Offline tests execute the functions in the exact bundle shipped by Docker. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import vm from 'node:vm';

const root = new URL('../', import.meta.url).pathname;
const directory = mkdtempSync(join(tmpdir(), 'aq-terminal-tests-'));
let bundle;
try {
  execFileSync('python3', ['scripts/check-dashboard-candidate.py', '--extract', directory],
    { cwd: root, env: { ...process.env, PYTHONDONTWRITEBYTECODE: '1' } });
  const file = readdirSync(join(directory, 'assets')).find(p => p.startsWith('InteractiveTerminal-') && p.endsWith('.js'));
  bundle = readFileSync(join(directory, 'assets', file), 'utf8');
  // Check the complete emitted module as well as the extracted functions below.
  execFileSync(process.execPath, ['--check', join(directory, 'assets', file)]);
} finally { rmSync(directory, { recursive: true, force: true }); }
const original = execFileSync('tar', ['xOf', 'vendor/aq-dashboard-v0.1.5.tar.gz', 'assets/InteractiveTerminal-Di3hRKTH.js'], { cwd: root, encoding: 'utf8' });
const slice = (s, a, b) => s.slice(s.indexOf(a), s.indexOf(b, s.indexOf(a)));
const rawFunctions = slice(bundle, 'function re(e,t,n)', 'var le=');
const manual = slice(bundle, 'function de(options', 'function E(e){if(!e)');
const resolve = slice(bundle, 'async function Le(id', 'function Re(e,t)');
const native = slice(bundle, 'function ze(e)', 'var j=l');
const id = n => `10000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const identity = n => ({ terminal_id: id(n), epoch: 1, session: { kind: 'remote',
  attempt_id: id(n + 10), session_id: id(n + 20), node_id: id(30), boot_id: id(31), assignment_generation: 1 } });
const info = (n = 1, enabled = false, mode = 'finite_v1') => ({ terminal: identity(n), session_revision: 2,
  input_mode: mode, terminal_policy: { input_enabled: enabled, max_input_bytes: 4096, max_pending_input_bytes: 32768 },
  disconnected: false, control: { terminal: identity(n), session_revision: 2, generation: 0,
    lease: null, fence_acknowledged: false, input_paused: false } });
const flush = async () => { for (let j = 0; j < 20; j++) await Promise.resolve(); };

function fixture({ enabled = false, mode = 'finite_v1', connectFailure = false, discoveryFailure = false, legacy = false } = {}) {
  const sockets = [], states = [], writes = [], events = [], requests = [], finite = [], timers = new Map();
  let timerId = 0, nextConnection = 0;
  class Socket {
    static OPEN = 1;
    readyState = 1; bufferedAmount = 0; sent = [];
    constructor(url) { this.url = new URL(url); sockets.push(this); }
    send(value) { this.sent.push(value); }
    close() { if (this.readyState !== 3) { this.readyState = 3; this.onclose?.(); } }
    frame(data) { this.onmessage?.({ data }); }
    ready() { this.frame(JSON.stringify({ kind: 'ready', server_time_ms: 100, columns: 120, rows: 40 })); }
  }
  const context = vm.createContext({ console, AbortController, AbortSignal, URL, URLSearchParams, ArrayBuffer, Uint8Array,
    performance, window: { location: { href: 'https://aq.example/agents' } }, WebSocket: Socket,
    crypto: { randomUUID: () => id(++nextConnection + 100) },
    setTimeout(fn, ms) { const key = ++timerId; timers.set(key, { fn, ms }); return key; },
    clearTimeout(key) { timers.delete(key); },
    i: v => v !== null && typeof v === 'object',
    n: async (...args) => { requests.push(args); if (connectFailure) throw Error('409'); return { generation: 7 }; },
    ve: async path => { if (discoveryFailure) throw Error('offline');
      const n = path.endsWith(id(2)) ? 2 : 1; return { body: info(n, enabled, mode) }; },
    ee: (raw, expected) => { assert.equal(raw.terminal.terminal_id, expected); return raw; },

    g: (a, b) => JSON.stringify(a) === JSON.stringify(b),
    v: value => value, // terminalIdentity: test identities above are fully populated.
    Ie: a => `/api/attempt-terminals/${a.id}/terminal`,
    Fe: options => { finite.push(options); return { close() {}, async release() {} }; },
  });
  vm.runInContext(rawFunctions + slice(original, 'function E(e){if(!e)', 'function pe(e,t,n)') + manual + (legacy ? slice(original, 'async function Le(e,t,r)', 'function Re(e,t)') : resolve) +
    slice(bundle, 'function Re(e,t)', 'function ze(e)') + native + ';globalThis.api={de,Le,ze}', context);
  const options = (n = 1) => ({ terminalId: id(n), allowControl: true,
    write: async bytes => { writes.push([...bytes]); events.push('write'); },
    reset: async () => { events.push('reset'); }, state: state => states.push(state) });
  return { ...context.api, options, sockets, states, writes, events, requests, finite, timers,
    fire(ms) { const entries = [...timers].filter(([, t]) => t.ms === ms);
      for (const [key, t] of entries) { timers.delete(key); t.fn(); } } };
}

test('reproduces the pinned discovery failure: a refused preflight silently selects the input-disabled finite relay', async () => {
  const f = fixture({ legacy: true, connectFailure: true });
  f.ze(f.options()); await flush();
  assert.equal(f.requests.length, 1);
  assert.equal(f.finite.length, 1);
  assert.equal(f.sockets.length, 0);
});

test('task selection resolves terminal to attempt and acquires control once, only on its live path', async () => {
  const f = fixture(); const client = f.ze(f.options()); await flush();
  assert.equal(f.finite.length, 0); assert.equal(f.requests.length, 1);
  const socket = f.sockets[0];
  assert.equal(socket.url.pathname, `/api/attempt-terminals/${id(11)}/terminal/live`);
  assert.equal(f.requests[0][1], `/api/attempt-terminals/${id(11)}/terminal/connect`);
  assert.equal(socket.url.searchParams.get('client_id'), f.requests[0][2].client_id);
  assert.equal(socket.url.searchParams.get('generation'), '7');
  assert.equal(socket.url.searchParams.has('columns'), false);
  socket.ready(); socket.frame(Uint8Array.of(65).buffer); await flush();
  socket.frame(Uint8Array.of(66).buffer); await flush();
  assert.deepEqual(f.events, ['reset', 'write', 'write']); assert.deepEqual(f.writes, [[65], [66]]);
  client.sendInput(Uint8Array.of(67)); client.resize(100, 30);
  assert.deepEqual([...socket.sent[0]], [67]); assert.equal(JSON.parse(socket.sent[1]).kind, 'resize');
  assert.equal(f.events.filter(e => e === 'reset').length, 1); client.close();
});

test('a refused live control acquisition stays explicitly unavailable without silently changing transports', async () => {
  const f = fixture({ connectFailure: true }); const client = f.ze(f.options()); await flush();
  assert.equal(f.finite.length, 0); assert.equal(f.states.at(-1).connected, false);
  assert.match(f.states.at(-1).message, /unavailable/); client.close();
});

test('discovery failure reports an explicit retry state', async () => {
  const f = fixture({ discoveryFailure: true }); const client = f.ze(f.options()); await flush();
  assert.equal(f.finite.length, 0); assert.match(f.states.at(-1).message, /unavailable/); client.close();
});

test('output-only epoch relay metadata can resolve to a live attempt without enabling finite input', async () => {
  const f = fixture({ mode: 'epoch_v1' }); const client = f.ze(f.options()); await flush();
  assert.equal(f.sockets.length, 1); assert.equal(f.finite.length, 0); client.close();
});

test('finite input retains its existing transport; unsupported keyboard modes fail visibly', async () => {
  const f = fixture({ enabled: true }); const client = f.ze(f.options()); await flush();
  assert.equal(f.requests.length, 0); assert.equal(f.finite.length, 1); client.close();
  const unsupported = fixture({ enabled: true, mode: 'epoch_v1' });
  const second = unsupported.ze(unsupported.options()); await flush();
  assert.equal(unsupported.finite.length, 0); assert.match(unsupported.states.at(-1).message, /unavailable/); second.close();
});

test('binding changes cannot route a reconnect to another attempt', async () => {
  const f = fixture(); const options = f.options();
  options.expect = { terminal: identity(2), session_revision: 2 };
  const client = f.ze(options); await flush();
  assert.equal(f.requests.length, 0); assert.equal(f.sockets.length, 0); assert.match(f.states.at(-1).message, /unavailable/); client.close();
});

test('ready without bytes exposes empty state; silent attachment and repaint have bounded failure states', async () => {
  const f = fixture(); const client = f.ze(f.options()); await flush();
  f.fire(5000); assert.match(f.states.at(-1).message, /timed out/); client.close();
  const second = fixture(); const c = second.ze(second.options()); await flush(); second.sockets[0].ready(); await flush();
  assert.match(second.states.at(-1).message, /waiting for terminal output/);
  assert.equal(second.timers.size, 0); c.repaint(); second.fire(5000);
  assert.match(second.states.at(-1).message, /repaint timed out/); c.close();
});

test('switching agents and returning discards stale callbacks, queued output and listeners', async () => {
  const f = fixture(); const first = f.ze(f.options()); await flush();
  const old = f.sockets[0]; old.ready(); old.frame(Uint8Array.of(65).buffer); first.close();
  const second = f.ze(f.options(2)); await flush(); const next = f.sockets[1];
  assert.equal(next.url.pathname, `/api/attempt-terminals/${id(12)}/terminal/live`);
  old.frame(Uint8Array.of(99).buffer); next.ready(); next.frame(Uint8Array.of(66).buffer); await flush();
  assert.deepEqual(f.writes, [[66]]); second.close();
  const returned = f.ze(f.options()); await flush(); f.sockets[2].ready(); f.sockets[2].frame(Uint8Array.of(67).buffer); await flush();
  assert.deepEqual(f.writes, [[66], [67]]); returned.close(); assert.equal(f.timers.size, 0);
});

test('a worker reconnect before ready reuses the acquired generation and never replays input', async () => {
  const f = fixture(); const client = f.ze(f.options()); await flush();
  const old = f.sockets[0]; client.sendInput(Uint8Array.of(65)); old.close(); f.fire(200);
  const next = f.sockets[1]; assert.equal(next.url.search, old.url.search); next.ready(); await flush();
  old.frame(Uint8Array.of(99).buffer); client.sendInput(Uint8Array.of(66));
  assert.equal(f.requests.length, 1); assert.deepEqual(next.sent.map(v => [...v]), [[66]]); client.close();
});

test('established disconnect freezes input; explicit reconnect restores replay and interaction', async () => {
  const f = fixture(); const client = f.ze(f.options()); await flush(); f.sockets[0].ready(); await flush();
  f.sockets[0].close(); client.sendInput(Uint8Array.of(65)); assert.equal(f.sockets[0].sent.length, 0);
  assert.equal(f.states.at(-1).controlling, false); assert.match(f.states.at(-1).message, /Disconnected/); client.close();
  const restored = f.ze(f.options()); await flush(); f.sockets[1].ready(); f.sockets[1].frame(Uint8Array.of(66).buffer); await flush();
  restored.sendInput(Uint8Array.of(67)); assert.deepEqual(f.writes, [[66]]); assert.deepEqual([...f.sockets[1].sent[0]], [67]); restored.close();
});

test('watchers never send input, Escape or resize; current controllers can resize without clearing output', async () => {
  const f = fixture(); const options = f.options(); options.allowControl = false;
  const client = f.ze(options); await flush(); const socket = f.sockets[0]; socket.ready(); await flush();
  assert.equal(socket.url.searchParams.get('read_only'), 'true');
  client.sendInput(Uint8Array.of(65)); client.sendEscape(); client.resize(80, 24);
  assert.equal(socket.sent.length, 0); assert.equal(f.states.at(-1).controlling, false); client.close();
});

test('closed discovery cannot create a late socket or acquire control', async () => {
  const f = fixture(); const client = f.ze(f.options()); client.close(); await flush();
  assert.equal(f.requests.length, 0); assert.equal(f.sockets.length, 0);
});


test('disconnect during discovery cancels the pending attachment', async () => {
  const f = fixture(); const client = f.ze(f.options()); await client.release(); await flush();
  assert.equal(f.requests.length, 0); assert.equal(f.sockets.length, 0); client.close();
});
