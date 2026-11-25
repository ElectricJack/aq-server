// Bundle bindings: ve=terminalJson, ee=adaptTerminalInfo, i=isRecord,
// g=sameTerminal. Discovery is read-only; the chosen client alone acquires control.
async function Le(id, signal, expect) {
  const raw = (await ve(`/api/v1/terminals/${encodeURIComponent(id)}`, signal)).body;
  const info = ee(raw, id);
  if (expect && (!g(expect.terminal, info.terminal) || expect.session_revision !== info.session_revision)) {
    throw Error('Terminal binding changed');
  }
  if (!i(raw.terminal_policy) || typeof raw.terminal_policy.input_enabled !== 'boolean') {
    throw Error('Invalid terminal policy');
  }
  if (info.terminal.session.kind !== 'remote') return null;
  // Finite keyboard mutations are supported only for finite_v1. Output-only
  // remote attempts use their live socket regardless of the relay input mode.
  if (raw.terminal_policy.input_enabled && raw.input_mode !== 'finite_v1') {
    throw Error('Unsupported interactive terminal mode');
  }
  return { id: info.terminal.session.attempt_id, finiteInput: raw.terminal_policy.input_enabled };
}
