// Local replacement for connectManualTerminal in dashboard v0.1.5.
// Bundle bindings: n=apiSend, i=isRecord, ie=readyFrame, b=fixedGrid,
// ce=terminalInputChunks, x=rawChunks. No input is replayed on reconnect.
function de(options, path = `/api/manual-agents/${options.terminalId}/terminal`, mode = 'manual') {
  const fixed = mode !== 'manual', readOnly = mode === 'attempt-watch';
  const abort = new AbortController(), client = crypto.randomUUID();
  let socket = null, closed = false, connected = false;
  let paused = options.initiallyPaused ?? false, repainting = false;
  let generation = 0, columns = 80, rows = 24, pending = 0, tries = 0;
  let observed = null, clock, grid, failure = null;
  let writing = Promise.resolve(), didConnect = false, retry, deadline;
  const connectedMessage = readOnly ? 'Connected' : 'Connected — ready for typing';
  const current = ws => !closed && socket === ws;
  const canSend = () => connected && !paused && !repainting;
  function emit(message) {
    if (!closed) options.state({ message, connected, controlling: !readOnly && canSend(),
      canControl: !readOnly && connected, paused, outputObservedAtMs: observed,
      clock, grid, canRepaint: fixed && connected && !repainting });
  }
  function freeze(message) {
    if (closed) return;
    failure ??= message;
    paused = true;
    connected = false;
    clearTimeout(deadline);
    clearTimeout(retry);
    socket?.close();
    emit(failure);
  }
  async function connect() {
    if (paused) { emit('Reconnect to inspect the same agent before typing'); return; }
    emit('Connecting to the agent terminal…');
    try {
      const value = await n('POST', `${path}/connect`, { client_id: client },
        AbortSignal.any([abort.signal, AbortSignal.timeout(5000)]));
      if (closed) return;
      if (!i(value) || !Number.isSafeInteger(value.generation)) throw Error('Invalid terminal connection');
      generation = Number(value.generation);
      openSocket();
    } catch {
      if (!closed) freeze('Agent terminal unavailable — reconnect to the same agent');
    }
  }
  function ready(ws, text) {
    let frame;
    try { frame = ie(text); } catch { freeze('Invalid terminal response — reconnect'); return; }
    const dimensions = fixed ? b(frame) : undefined;
    if (fixed && !dimensions) { freeze('Invalid terminal dimensions — reconnect'); return; }
    clearTimeout(deadline);
    // Serialize reset, readiness and paint, including a repaint's old queued bytes.
    writing = writing.then(async () => {
      if (!current(ws) || paused) return;
      grid = dimensions;
      await options.reset();
      if (!current(ws) || paused || ws.readyState !== WebSocket.OPEN) return;
      observed = null;
      clock = { serverMs: Number(frame.server_time_ms), at: performance.now() };
      didConnect = true;
      connected = true;
      repainting = false;
      emit('Connected — waiting for terminal output');
      if (!fixed) ws.send(JSON.stringify({ kind: 'resize', columns, rows }));
    }).catch(() => freeze('Terminal repaint interrupted — reconnect'));
  }
  function output(ws, data) {
    if (!(data instanceof ArrayBuffer) || data.byteLength > 16384) {
      freeze('Terminal output exceeded its bound — reconnect'); return;
    }
    const bytes = new Uint8Array(data);
    pending += bytes.length;
    if (pending > 1048576) { freeze('Terminal display fell behind — reconnect'); return; }
    writing = writing.then(async () => {
      try {
        if (!current(ws) || paused || !connected) return;
        await options.write(bytes);
        if (!current(ws) || paused) return;
        observed = clock.serverMs + performance.now() - clock.at;
        emit(connectedMessage);
      } finally { pending -= bytes.length; }
    }).catch(() => freeze('Terminal display interrupted — reconnect'));
  }
  function openSocket() {
    if (closed || paused) return;
    const url = new URL(`${path}/live`, window.location.href);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    const params = new URLSearchParams({ client_id: client, generation: String(generation) });
    if (fixed) params.set('read_only', String(readOnly));
    else { params.set('columns', String(columns)); params.set('rows', String(rows)); }
    url.search = params.toString();
    const ws = new WebSocket(url);
    socket = ws;
    ws.binaryType = 'arraybuffer';
    deadline = setTimeout(() => { if (current(ws)) freeze('Agent terminal attachment timed out — reconnect'); }, 5000);
    ws.onmessage = event => {
      if (!current(ws)) return;
      if (typeof event.data === 'string') ready(ws, event.data);
      else output(ws, event.data);
    };
    ws.onerror = () => { if (current(ws) && didConnect) freeze('Terminal connection interrupted — reconnect to the same agent'); };
    ws.onclose = () => {
      if (!current(ws) || paused) return;
      clearTimeout(deadline);
      connected = false;
      if (!didConnect && tries < 5) {
        emit('Connecting to the agent terminal…');
        retry = setTimeout(openSocket, Math.min(2000, 200 * 2 ** tries++));
      } else freeze('Disconnected — reconnect to the same agent');
    };
  }
  function activeSocket() {
    return !closed && canSend() && socket?.readyState === WebSocket.OPEN ? socket : null;
  }
  function sendInput(bytes) {
    if (readOnly || !bytes.length) return;
    const ws = activeSocket();
    if (!ws) return;
    if (ws.bufferedAmount + bytes.length > 65536) { freeze('Terminal input connection fell behind — reconnect'); return; }
    const chunks = fixed ? ce(bytes, 4096) : x(bytes);
    if (!chunks) { freeze('Incomplete terminal input — reconnect to type'); return; }
    for (const chunk of chunks) ws.send(chunk);
  }
  void connect();
  return {
    async takeControl() {},
    async release() { paused = true; connected = false; clearTimeout(deadline); clearTimeout(retry); socket?.close(); },
    sendInput,
    sendEscape() { if (!readOnly) activeSocket()?.send(fixed ? JSON.stringify({ kind: 'escape' }) : Uint8Array.of(27)); },
    resize(c, r) {
      if (readOnly || ![c, r].every(Number.isFinite)) return;
      columns = Math.max(2, Math.min(500, Math.round(c)));
      rows = Math.max(1, Math.min(300, Math.round(r)));
      activeSocket()?.send(JSON.stringify({ kind: 'resize', columns, rows }));
    },
    repaint() {
      const ws = activeSocket();
      if (!fixed || !ws) return;
      repainting = true;
      emit('Repainting terminal…');
      deadline = setTimeout(() => freeze('Terminal repaint timed out — reconnect'), 5000);
      ws.send(JSON.stringify({ kind: 'repaint' }));
    },
    close() {
      closed = true;
      clearTimeout(retry);
      clearTimeout(deadline);
      abort.abort();
      socket?.close();
      socket = null;
      connected = false;
    },
  };
}
