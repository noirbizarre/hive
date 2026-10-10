// Browser transport adapter: JSON-RPC over the same SDK-served ACPv2 /acp endpoint.
export function connect(url, onUpdate, onStatus) {
  const ws = new WebSocket(url);
  let nextId = 0;
  const pending = new Map();
  const store = typeof localStorage === 'undefined' ? null : localStorage;
  let sessionId = store?.getItem('hive.session') || undefined;
  let selectedAgent = store?.getItem('hive.agent') || '';
  let lastSeq = Number(store?.getItem(`hive.seq.${sessionId}`) || 0);
  let owner;
  let active = false;
  let uncertain = JSON.parse(store?.getItem(`hive.pending.${sessionId}`) || 'null');
  let rejectInitialize;
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
  });
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (message.method === 'session/update' && message.params?.sessionId === sessionId) {
      const seq = message.params._meta?.hive?.seq;
      if (typeof seq !== 'number' || seq > lastSeq) {
        if (typeof seq === 'number') { lastSeq = seq; store?.setItem(`hive.seq.${sessionId}`, String(seq)); }
        active = message.params.update.sessionUpdate === 'state_update'
          ? message.params.update.state !== 'idle' : active;
        onUpdate(message.params.update);
      }
    }
    if (message.id != null && pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) reject(new Error(message.error.data || message.error.message)); else resolve(message.result);
    }
  };
  ws.onclose = () => { rejectInitialize?.(new Error('Hive disconnected')); for (const { reject } of pending.values()) reject(new Error('Hive disconnected')); pending.clear(); onStatus('Disconnected'); };
  ws.onerror = () => { rejectInitialize?.(new Error('Hive connection failed')); onStatus('Hive connection failed'); };
  const initialized = new Promise((resolve, reject) => {
    rejectInitialize = reject;
    ws.onopen = async () => {
      try {
        const result = await request('initialize', { protocolVersion: 2, info: { name: 'hive-web', version: '0.1.0' }, capabilities: {} });
        if (result.protocolVersion !== 2 || !result.capabilities?.session) throw new Error('ACPv2 sessions unavailable');
        if (sessionId) {
          const resume = await request('session/resume', { sessionId, cwd: '/', replayFrom: lastSeq
            ? { type: '_hive_seq', seq: lastSeq + 1 } : { type: 'start' } });
          owner = resume._meta?.hive;
          if (!owner) throw new Error('Hive did not return controller metadata');
          active = owner.running;
        }
        onStatus(sessionId ? `Controlling ${sessionId}${active ? ' (running)' : ''}` : 'Connected to ACPv2');
        rejectInitialize = undefined;
        resolve(result);
      } catch (error) { onStatus(error.message); reject(error); }
    };
  });
  return {
    initialized,
    async prompt(text, agent) {
      await initialized;
      if (sessionId && (agent || '') !== selectedAgent) throw new Error('Agent selection applies to a new session; reload to choose another agent');
       if (!sessionId) {
         const created = await request('session/new', { cwd: '/', mcpServers: [], ...(agent ? { _meta: { hive: { agent } } } : {}) });
         sessionId = created.sessionId;
         selectedAgent = agent || '';
         owner = created._meta?.hive;
         if (!owner) throw new Error('Hive did not return controller metadata');
         store?.setItem('hive.session', sessionId);
         store?.setItem('hive.agent', selectedAgent);
       }
       if (uncertain && uncertain.text !== text) throw new Error('The previous prompt acknowledgement is uncertain; retry that text after reconnect');
       if (active && !uncertain) throw new Error('Wait for the active generation to become idle');
       const key = uncertain?.key || crypto.randomUUID();
       uncertain = { key, text };
       store?.setItem(`hive.pending.${sessionId}`, JSON.stringify(uncertain));
       active = true;
       const response = await request('session/prompt', { sessionId, prompt: [{ type: 'text', text }], _meta: { hive: { token: owner.token, generation: owner.generation, key } } });
       uncertain = null;
       store?.removeItem(`hive.pending.${sessionId}`);
       return response;
     },
    cancel() { if (sessionId && owner && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ jsonrpc: '2.0', method: 'session/cancel', params: { sessionId, _meta: { hive: { token: owner.token, generation: owner.generation } } } })); },
    sessionId() { return sessionId; },
    close() { ws.close(); }
  };
}

if (typeof document !== 'undefined') {
  const requestedSession = new URLSearchParams(location.search).get('session');
  if (requestedSession && requestedSession !== localStorage.getItem('hive.session')) {
    localStorage.setItem('hive.session', requestedSession);
    localStorage.removeItem('hive.agent');
  }
  const status = document.querySelector('#status');
  const output = document.querySelector('#output');
  fetch('/hive/v1/status').then(r => r.json()).then(s => { status.textContent = `Hive ${s.version} (${s.sessions} sessions)`; }).catch(() => { status.textContent = 'Daemon unavailable'; });
  const agentSelect = document.querySelector('#agent');
  fetch('/hive/v1/providers').then(r => r.json()).then(({ agents }) => {
    for (const agent of agents) agentSelect.add(new Option(agent.name, agent.id));
    agentSelect.insertBefore(new Option('Default agent', ''), agentSelect.firstChild);
    agentSelect.value = '';
  }).catch(() => { status.textContent = 'Agent inventory unavailable'; });
  const client = connect(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/acp`, update => {
    if (update.sessionUpdate === 'agent_message_chunk') output.textContent += update.content?.text || '';
  }, message => { status.textContent = message; });
  document.querySelector('#prompt').onsubmit = async event => {
    event.preventDefault();
    try { await client.prompt(new FormData(event.target).get('text'), agentSelect.value); agentSelect.disabled = true; } catch (error) { status.textContent = error.message; }
  };
  document.querySelector('#cancel').onclick = () => client.cancel();
  document.querySelector('#takeover').onsubmit = event => {
    event.preventDefault();
    const id = new FormData(event.target).get('session').trim();
    if (id) location.search = new URLSearchParams({ session: id }).toString();
  };
}
