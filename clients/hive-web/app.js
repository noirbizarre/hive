// The daemon owns session lifecycle and control; this adapter only keeps a view of its event log.
export function conversation() {
  const lines = [];
  const messages = new Map();
  const tools = new Map();
  let seq = 0;
  let running = false;
  let generation = 0;
  let pilot = false;

  function upsert(map, key, kind, text, replace = false) {
    if (!map.has(key)) {
      map.set(key, lines.length);
      lines.push({ kind, text: '' });
    }
    const line = lines[map.get(key)];
    line.text = replace ? text : line.text + text;
  }

  function apply(event) {
    const number = event._meta?.hive?.seq;
    if (!Number.isSafeInteger(number) || number <= seq) return false;
    const update = event.update;
    const kind = update.sessionUpdate;
    if (kind === 'agent_message_chunk' || kind === 'user_message_chunk') {
      const speaker = kind.startsWith('agent') ? 'agent' : 'user';
      upsert(messages, `${speaker}:${update.messageId}`, speaker, update.content?.text || '');
    } else if (kind === 'agent_message' || kind === 'user_message') {
      const speaker = kind.startsWith('agent') ? 'agent' : 'user';
      upsert(messages, `${speaker}:${update.messageId}`, speaker,
        (update.content || []).map(block => block.text || `[${block.type || 'content'}]`).join('\n'), true);
    } else if (kind === 'tool_call_update' || kind === 'tool_call_content_chunk' || kind.startsWith('_hive_v1_tool')) {
      const tool = update.tool || update;
      upsert(tools, tool.toolCallId || 'tool', 'tool',
        kind === 'tool_call_content_chunk' ? JSON.stringify(tool) : `${tool.title || 'Tool'} — ${tool.status || ''} ${JSON.stringify(tool.content || '')}`,
        kind !== 'tool_call_content_chunk');
    } else if (kind === 'state_update') {
      if (update.state === 'running') running = true;
      if (update.state === 'idle') running = false;
    } else if (kind === '_hive_control') {
      generation = update.generation;
      pilot = update.pilot;
    }
    seq = number;
    return true;
  }

  return {
    apply,
    notice(text) { lines.push({ kind: 'notice', text }); },
    skip(nextSeq) { seq = nextSeq - 1; },
    running(value) { running = value; },
    get seq() { return seq; },
    get runningNow() { return running; },
    get generation() { return generation; },
    get pilot() { return pilot; },
    get lines() { return lines.map(line => ({ ...line })); },
  };
}

export function connect(url, onChange = () => {}) {
  const store = typeof localStorage === 'undefined' ? null : localStorage;
  let socket;
  let pending = new Map();
  let nextId = 0;
  let retryTimer;
  let closed = false;
  let connected = false;
  let selected;
  let history = conversation();
  let owner;
  let interaction;
  let earlyInteraction;
  let takingControl;
  let gap = false;
  let status = 'Connecting…';
  let uncertain;
  let creationUncertain = store?.getItem('hive.creating') === 'true';
  let openQueue = Promise.resolve();
  let openVersion = 0;
  let firstResolve;
  const initialized = new Promise(resolve => { firstResolve = resolve; });

  const snapshot = () => ({ connected, status, session: selected && { ...selected }, role: owner?.role || 'observer',
    generation: owner?.generation,
    running: history.runningNow, lines: history.lines, seq: history.seq, gap, interaction,
    uncertain: uncertain && { ...uncertain }, takingControl: Boolean(takingControl) });
  const publish = () => onChange(snapshot());
  const errorText = error => error?.data || error?.message || String(error);
  function request(method, params) {
    if (!connected || socket.readyState !== WebSocket.OPEN) return Promise.reject(new Error('Hive disconnected'));
    const id = ++nextId;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
    });
  }
  function clearInteraction() { interaction = undefined; publish(); }
  function relinquished() {
    owner = undefined;
    interaction = undefined;
    earlyInteraction = undefined;
    status = `Read-only ${selected?.sessionId || ''} — control transferred`;
    publish();
  }
  function update(message) {
    if (message.params?.sessionId !== selected?.sessionId) return;
    if (!history.apply({ ...message.params, _meta: message.params._meta })) return;
    if (owner?.role === 'pilot' && history.generation > owner.generation) relinquished();
    else publish();
  }
  function inbound(message) {
    if (message.method !== 'session/request_permission' && message.method !== 'elicitation/create') return;
    if (!selected || !connected || message.params?.sessionId !== selected.sessionId) return;
    if (takingControl && owner?.role !== 'pilot') { earlyInteraction = message; return; }
    if (owner?.role !== 'pilot') return;
    // A new pilot receives unanswered requests again; only the current generation may respond.
    interaction = { id: message.id, method: message.method, params: message.params, generation: owner.generation };
    publish();
  }
  function handle(message) {
    if (message.method === 'session/update') update(message);
    else if (message.method && message.id != null) inbound(message);
    if (message.id != null && pending.has(message.id)) {
      const task = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) task.reject(new Error(errorText(message.error))); else task.resolve(message.result);
    }
  }
  function ownership(meta) {
    const hive = meta?._meta?.hive;
    if (!hive || !Number.isSafeInteger(hive.generation) || !hive.role) throw new Error('Hive did not return control metadata');
    owner = hive.role === 'pilot' ? hive : { generation: hive.generation, role: 'observer' };
    history.running(hive.running);
    gap = false;
    status = `${owner.role === 'pilot' ? 'Controlling' : 'Read-only'} ${selected.sessionId}${hive.running ? ' (running)' : ''}`;
    publish();
    if (owner.role === 'pilot' && earlyInteraction) inbound(earlyInteraction);
    earlyInteraction = undefined;
    return hive;
  }
  async function resume(mode = 'observe', replay = true) {
    if (!selected) throw new Error('Select a session first');
    const target = selected.sessionId;
    const targetCwd = selected.cwd;
    const version = openVersion;
    const params = { sessionId: selected.sessionId, cwd: selected.cwd,
      ...(mode === 'observe' ? { _meta: { hive: { mode } } } : {}) };
    if (replay) params.replayFrom = history.seq
      ? { type: '_hive_seq', seq: history.seq + 1 } : { type: 'start' };
    try {
      const response = await request('session/resume', params);
      // A delayed response from an older attachment must never grant control over the new view.
      if (selected?.sessionId !== target || version !== openVersion) {
        const stale = response?._meta?.hive;
        if (stale?.role === 'pilot' && stale.token) {
          // An overtaken handoff may succeed at the daemon after the user has left its view.
          await request('session/resume', { sessionId: target, cwd: targetCwd,
            _meta: { hive: { mode: 'relinquish', generation: stale.generation, token: stale.token } } });
        }
        return response;
      }
      const hive = ownership(response);
      if (!replay) {
        // With no retained events, move the cursor to the daemon's atomic attachment boundary.
        history.skip(hive.nextSeq);
        history.notice('[Earlier history unavailable]');
        publish();
      }
      return response;
    } catch (error) {
      if (selected?.sessionId !== target || version !== openVersion) throw error;
      if (error.message.includes('replay cursor unavailable')) {
        gap = true;
        status = 'History expired — attach without earlier events to continue';
      } else status = error.message;
      publish();
      throw error;
    }
  }
  function openSocket() {
    if (closed) return;
    const ws = new WebSocket(url);
    socket = ws;
    pending = new Map();
    nextId = 0;
    ws.onmessage = event => { if (socket === ws) handle(JSON.parse(event.data)); };
    ws.onopen = async () => {
      if (socket !== ws) return;
      connected = true;
      try {
        const response = await request('initialize', { protocolVersion: 2,
          info: { name: 'hive-web', version: '0.1.0' },
          capabilities: { elicitation: { form: {}, url: {} } } });
        if (response.protocolVersion !== 2 || !response.capabilities?.session) throw new Error('ACPv2 sessions unavailable');
        firstResolve(response);
        if (selected) await resume('observe');
        else {
          status = creationUncertain ? 'Session creation interrupted — select the original session from the list' : 'Connected to ACPv2';
          publish();
        }
      } catch (error) {
        status = error.message;
        publish();
        // An expired replay needs an explicit choice, but the transport is usable.
        if (!selected && ws.readyState === WebSocket.OPEN) ws.close();
      }
    };
    ws.onclose = () => {
      if (socket !== ws) return;
      connected = false;
      owner = undefined;
      interaction = undefined;
      earlyInteraction = undefined;
      takingControl = undefined;
      for (const task of pending.values()) task.reject(new Error('Hive disconnected'));
      pending.clear();
      status = closed ? 'Disconnected' : 'Disconnected — reconnecting to the same session…';
      publish();
      if (!closed) retryTimer = setTimeout(openSocket, 750);
    };
    ws.onerror = () => { if (socket === ws) { status = 'Hive connection failed'; publish(); } };
  }
  openSocket();
  publish();

  return {
    initialized,
    state: snapshot,
    open(session) {
      if (!session?.sessionId || !session.cwd) throw new Error('Select a listed Hive session with a cwd');
      const version = ++openVersion;
      const operation = openQueue.then(async () => {
        await initialized;
        if (version !== openVersion) return;
        if (selected?.sessionId === session.sessionId && owner) return;
        if (selected && owner?.role === 'pilot' && selected.sessionId !== session.sessionId) {
          // Leaving an owned conversation must release its pilot token without ending the agent session.
          await request('session/resume', { sessionId: selected.sessionId, cwd: selected.cwd,
            _meta: { hive: { mode: 'relinquish', generation: owner.generation, token: owner.token } } });
        }
        if (version !== openVersion) return;
        selected = { sessionId: session.sessionId, cwd: session.cwd, agent: session.agent };
        creationUncertain = false;
        store?.removeItem('hive.creating');
        history = conversation();
        owner = undefined;
        interaction = undefined;
        earlyInteraction = undefined;
        uncertain = JSON.parse(store?.getItem(`hive.pending.${selected.sessionId}`) || 'null');
        publish();
        return resume('observe');
      });
      openQueue = operation.catch(() => {});
      return operation;
    },
    async attachWithoutHistory() { if (!gap) throw new Error('No history gap to skip'); return resume('observe', false); },
    async takeControl() {
      if (!selected || !connected || gap) throw new Error('Attach to a session before taking control');
      if (owner?.role === 'pilot') return;
      if (!takingControl) {
        takingControl = resume('pilot').finally(() => { takingControl = undefined; publish(); });
        publish();
      }
      return takingControl;
    },
    async prompt(text, agent = '') {
      await initialized;
      if (!text?.trim()) throw new Error('Enter a prompt');
      if (!selected) {
        if (creationUncertain) throw new Error('Session creation was interrupted; choose the original session from the list before prompting');
        creationUncertain = true;
        store?.setItem('hive.creating', 'true');
        let created;
        try {
          created = await request('session/new', { cwd: '/', mcpServers: [],
            ...(agent ? { _meta: { hive: { agent } } } : {}) });
        } catch (error) {
          if (connected) { creationUncertain = false; store?.removeItem('hive.creating'); }
          throw error;
        }
        selected = { sessionId: created.sessionId, cwd: '/', agent };
        creationUncertain = false;
        store?.removeItem('hive.creating');
        history = conversation();
        ownership(created);
      }
      if (owner?.role !== 'pilot') throw new Error('Start typing to take control first');
      if (uncertain && uncertain.text !== text) throw new Error('Retry the uncertain prompt with its original text first');
      if (history.runningNow && !uncertain) throw new Error('Wait for the active generation to become idle');
      const key = uncertain?.key || crypto.randomUUID();
      uncertain = { key, text };
      store?.setItem(`hive.pending.${selected.sessionId}`, JSON.stringify(uncertain));
      history.running(true);
      publish();
      try {
        const result = await request('session/prompt', { sessionId: selected.sessionId, prompt: [{ type: 'text', text }],
          _meta: { hive: { generation: owner.generation, token: owner.token, key } } });
        uncertain = undefined;
        store?.removeItem(`hive.pending.${selected.sessionId}`);
        publish();
        return result;
      } catch (error) {
        status = `Prompt acknowledgement uncertain: ${error.message}; retry with the same key`;
        if (error.message.includes('stale')) relinquished();
        publish();
        throw error;
      }
    },
    cancel() {
      if (!connected || owner?.role !== 'pilot') return false;
      socket.send(JSON.stringify({ jsonrpc: '2.0', method: 'session/cancel', params: { sessionId: selected.sessionId,
        _meta: { hive: { generation: owner.generation, token: owner.token } } } }));
      if (interaction) this.respond(interaction.method === 'elicitation/create'
        ? { action: 'cancel' } : { outcome: { outcome: 'cancelled' } }, interaction.id);
      return true;
    },
    respond(result, id) {
      if (!connected || owner?.role !== 'pilot' || !interaction || interaction.id !== id ||
          interaction.generation !== owner.generation) return false;
      socket.send(JSON.stringify({ jsonrpc: '2.0', id, result }));
      clearInteraction();
      return true;
    },
    close() { closed = true; clearTimeout(retryTimer); socket?.close(); },
  };
}

// Catalog revisions are invalidations, never conversation updates or browser ownership state.
export function catalog(onChange, onError = () => {}) {
  let revision = -1;
  let stopped = false;
  let requestId = 0;
  const events = new EventSource('/hive/v1/sessions/events');
  async function refresh() {
    const id = ++requestId;
    try {
      const response = await fetch('/hive/v1/sessions');
      if (!response.ok) throw new Error(`Session list failed (${response.status})`);
      const snapshot = await response.json();
      if (stopped || id !== requestId) return;
      const newer = revision > snapshot.revision;
      revision = Math.max(revision, snapshot.revision);
      onChange(snapshot.sessions);
      if (newer) queueMicrotask(refresh);
    } catch (error) { if (!stopped) onError(error.message); }
  }
  events.addEventListener('revision', event => {
    const next = Number(event.data);
    if (Number.isSafeInteger(next) && next > revision) { revision = next; refresh(); }
  });
  events.onopen = () => refresh();
  refresh();
  return { refresh, close() { stopped = true; events.close(); } };
}

function add(parent, tag, text, attributes = {}) {
  const element = document.createElement(tag);
  element.textContent = text;
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, value);
  parent.append(element);
  return element;
}

function formValues(schema, form) {
  const content = {};
  for (const [name, field] of Object.entries(schema?.properties || {})) {
    const input = form.elements.namedItem(name);
    if (field.type === 'boolean') {
      if (input.value !== '') content[name] = input.value === 'true';
    } else if (input.value !== '') {
      if (field.type === 'integer' || field.type === 'number') {
        const number = Number(input.value);
        if (!Number.isFinite(number) || (field.type === 'integer' && !Number.isInteger(number))) throw new Error(`${name} needs a ${field.type}`);
        content[name] = number;
      } else if (field.type === 'array') content[name] = input.value.split(',').map(part => part.trim());
      else content[name] = input.value;
    }
  }
  for (const name of schema?.required || []) {
    if (!(name in content)) throw new Error(`Required field: ${name}`);
  }
  return content;
}

export function mount(client, root = document) {
  const status = root.querySelector('#status');
  const list = root.querySelector('#sessions');
  const listStatus = root.querySelector('#session-list-status');
  const toggle = root.querySelector('#toggle-sessions');
  const layout = root.querySelector('#layout');
  const output = root.querySelector('#output');
  const prompt = root.querySelector('#prompt');
  const input = prompt.elements.namedItem('text');
  const send = root.querySelector('#send');
  const agentSelect = root.querySelector('#agent');
  const cancel = root.querySelector('#cancel');
  const dialog = root.querySelector('#interaction');
  const retry = root.querySelector('#retry');
  const skip = root.querySelector('#skip-history');
  let sessions = [];
  let recent = [];
  let navigationVisible = true;
  let selectedBefore;
  let opening;
  let sending = false;
  let currentInteraction;

  function show(state) {
    status.textContent = state.status;
    if (state.session?.sessionId !== selectedBefore) {
      selectedBefore = state.session?.sessionId;
      if (selectedBefore) {
        recent = [selectedBefore, ...recent.filter(id => id !== selectedBefore)];
        history.replaceState?.(null, '', `?${new URLSearchParams({ session: selectedBefore })}`);
      }
    }
    renderRows(state);
    const pilot = state.role === 'pilot' && state.connected && !state.gap;
    input.disabled = !state.connected || state.gap;
    send.disabled = !state.connected || state.gap || sending || state.takingControl || state.running ||
      (Boolean(state.session) && !pilot) || Boolean(state.uncertain);
    cancel.disabled = !pilot || !state.running;
    retry.hidden = !state.uncertain;
    retry.disabled = !pilot || !state.connected || sending;
    skip.hidden = !state.gap;
    agentSelect.disabled = Boolean(state.session);
    if (state.session?.agent && Array.from(agentSelect.options).some(option => option.value === state.session.agent)) {
      agentSelect.value = state.session.agent;
    }
    output.replaceChildren();
    for (const line of state.lines) add(output, 'div', `${line.kind === 'notice' ? '' : `${line.kind}: `}${line.text}`);
    if (currentInteraction?.id === state.interaction?.id) return;
    currentInteraction = state.interaction;
    dialog.replaceChildren();
    dialog.hidden = !state.interaction || !pilot;
    if (dialog.hidden) return;
    const interaction = state.interaction;
    const params = interaction.params;
    add(dialog, 'p', params.title || params.message || 'Agent needs input');
    if (params.description) add(dialog, 'p', params.description);
    const form = add(dialog, 'form', '');
    if (interaction.method === 'session/request_permission') {
      for (const option of params.options || []) {
        const button = add(form, 'button', option.name, { type: 'button' });
        button.onclick = () => client.respond({ outcome: { outcome: 'selected', optionId: option.optionId } }, interaction.id);
      }
    } else if (params.mode === 'form') {
      for (const [name, field] of Object.entries(params.requestedSchema?.properties || {})) {
        const label = add(form, 'label', name);
        const control = add(label, 'input', '', { name, type: field.type === 'integer' || field.type === 'number' ? 'number' : 'text' });
        if (field.type === 'boolean') {
          const select = add(label, 'select', '', { name });
          control.remove();
          for (const value of ['', 'true', 'false']) add(select, 'option', value || 'Choose…', { value });
        }
      }
      const accept = add(form, 'button', 'Submit', { type: 'submit' });
      accept.disabled = false;
      form.onsubmit = event => {
        event.preventDefault();
        try { client.respond({ action: 'accept', content: formValues(params.requestedSchema, form) }, interaction.id); }
        catch (error) { status.textContent = error.message; }
      };
    } else if (params.mode === 'url') {
      try {
        const url = new URL(params.url);
        if (url.protocol === 'http:' || url.protocol === 'https:') add(form, 'a', url.href, { href: url.href, target: '_blank', rel: 'noopener noreferrer' });
      } catch { /* An invalid agent URL must not become an unsafe browser link. */ }
      add(form, 'button', 'Done', { type: 'button' }).onclick = () => client.respond({ action: 'accept' }, interaction.id);
    }
    add(form, 'button', 'Decline', { type: 'button' }).onclick = () => client.respond(
      interaction.method === 'session/request_permission' ? { outcome: { outcome: 'cancelled' } } : { action: 'decline' }, interaction.id);
  }

  async function open(id) {
    const session = sessions.find(item => item.sessionId === id);
    if (!session) { status.textContent = `Hive session ${id} not found; choose a listed session`; return; }
    try {
      await client.open(session);
    } catch (error) { status.textContent = error.message; }
  }
  const history = globalThis.history;
  const selectedId = new URLSearchParams(globalThis.location?.search || '').get('session');
  function renderRows(state) {
    const focused = root.activeElement?.dataset?.sessionId;
    list.replaceChildren();
    const ordered = [...sessions].sort((a, b) => {
      const first = recent.indexOf(a.sessionId);
      const second = recent.indexOf(b.sessionId);
      if (first !== -1 || second !== -1) return (first === -1 ? Infinity : first) - (second === -1 ? Infinity : second);
      return (b.lastActivityAt || 0) - (a.lastActivityAt || 0) || a.sessionId.localeCompare(b.sessionId);
    });
    for (const session of ordered) {
      const row = add(list, 'li', '');
      const button = add(row, 'button', session.title || session.sessionId, { type: 'button' });
      button.dataset.sessionId = session.sessionId;
      if (session.sessionId === state.session?.sessionId) button.setAttribute('aria-current', 'page');
      button.onclick = () => open(session.sessionId);
      const control = session.controlOwner
        ? session.sessionId === state.session?.sessionId && state.role === 'pilot' && state.connected
          && state.generation === session.generation ? 'controlled here' : 'controlled by another client'
        : 'unclaimed';
      const activity = session.lifecycle && session.lifecycle !== 'active' ? session.lifecycle : session.activity;
      const attention = session.sessionId === state.session?.sessionId && state.interaction ? ' · needs attention' : '';
      const detail = add(button, 'small', `${session.sessionId} · ${session.agent || 'unknown agent'} · ${activity || 'unknown'}${attention} · ${control} · ` +
        `${session.lastActivityAt == null ? 'no activity yet' : new Date(session.lastActivityAt).toLocaleString()}`);
      if (attention || activity === 'running' || activity === 'closing') detail.className = 'attention';
    }
    if (focused) list.querySelectorAll('button').forEach(button => { if (button.dataset.sessionId === focused) button.focus(); });
    if (listStatus) listStatus.textContent = sessions.length ? `${sessions.length} sessions` : 'No sessions yet. Send a prompt to start one.';
  }
  function renderList(items) {
    sessions = items;
    // Creation may publish before its catalog snapshot arrives; keep the new ID until then.
    recent = recent.filter(id => id === client.state().session?.sessionId || items.some(item => item.sessionId === id));
    renderRows(client.state());
    if (selectedId && !opening) { opening = true; open(selectedId); }
  }
  const feed = catalog(renderList, message => { if (listStatus) listStatus.textContent = message; else status.textContent = message; });
  if (toggle) toggle.onclick = () => {
    navigationVisible = !navigationVisible;
    layout.classList.toggle('nav-hidden', !navigationVisible);
    toggle.setAttribute('aria-expanded', String(navigationVisible));
    toggle.textContent = navigationVisible ? 'Hide sessions' : 'Show sessions';
    if (navigationVisible) list.querySelector('[aria-current="page"]')?.focus();
  };
  fetch('/hive/v1/providers').then(response => response.json()).then(({ agents }) => {
    for (const agent of agents) add(agentSelect, 'option', agent.name, { value: agent.id });
    if (client.state().session?.agent) agentSelect.value = client.state().session.agent;
  }).catch(() => { status.textContent = 'Agent inventory unavailable'; });
  root.querySelector('#takeover').onsubmit = event => {
    event.preventDefault();
    open(new FormData(event.target).get('session').trim());
  };
  input.oninput = () => {
    if (input.value && client.state().session && client.state().role !== 'pilot') {
      client.takeControl().catch(error => { status.textContent = error.message; });
    }
  };
  prompt.onsubmit = async event => {
    event.preventDefault();
    if (sending) return;
    sending = true;
    show(client.state());
    try {
      if (client.state().session) await client.takeControl();
      await client.prompt(input.value, agentSelect.value);
      input.value = '';
    } catch (error) { status.textContent = error.message; }
    finally { sending = false; show(client.state()); }
  };
  retry.onclick = async () => {
    sending = true;
    show(client.state());
    try { await client.prompt(client.state().uncertain.text); }
    catch (error) { status.textContent = error.message; }
    finally { sending = false; show(client.state()); }
  };
  skip.onclick = () => client.attachWithoutHistory().catch(error => { status.textContent = error.message; });
  cancel.onclick = () => client.cancel();
  show(client.state());
  return { show, close() { feed.close(); client.close(); } };
}

if (typeof document !== 'undefined') {
  let view;
  const client = connect(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/acp`, state => view?.show(state));
  view = mount(client);
}
