// Browser transport adapter: JSON-RPC over the same SDK-served ACPv2 /acp endpoint.
export function connect(url, onUpdate, onStatus) {
  const ws = new WebSocket(url);
  let nextId = 0;
  const pending = new Map();
  let sessionId;
  let selectedAgent;
  let rejectInitialize;
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
  });
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (message.method === 'session/update' && message.params?.sessionId === sessionId) onUpdate(message.params.update);
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
        onStatus('Connected to ACPv2');
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
      }
      return request('session/prompt', { sessionId, prompt: [{ type: 'text', text }] });
    },
    cancel() { if (sessionId && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ jsonrpc: '2.0', method: 'session/cancel', params: { sessionId } })); },
    close() { ws.close(); }
  };
}

if (typeof document !== 'undefined') {
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
}
