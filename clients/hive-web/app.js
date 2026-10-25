// The daemon owns lifecycle and control; this adapter projects transcript snapshots and live events.
export function conversation() {
  const lines = [];
  const messages = new Map();
  const tools = new Map();
  let seq = 0;
  let running = false;
  let generation = 0;
  let pilot = false;
  let activity = { version: 1, seq: 0, session: 'active', foreground: 'unknown', tools: {}, plans: {}, tasks: {},
    support: { tools: 'unknown', plans: 'unknown', tasks: 'unknown' } };
  let usage = {};
  let usageSeq = 0;
  let configOptions = [];
  let configSeq = 0;
  let gap = false;

  function reconcile(snapshot) {
    if (snapshot?.version === 1 && snapshot.seq >= activity.seq) activity = structuredClone(snapshot);
    gap = false;
  }

  function reconcileUsage(snapshot, number) {
    if (snapshot && Number.isSafeInteger(number) && number >= usageSeq) {
      usage = structuredClone(snapshot);
      usageSeq = number;
    }
  }

  function upsert(map, key, kind, text, replace = false) {
    if (!map.has(key)) {
      map.set(key, lines.length);
      lines.push({ kind, text: '' });
    }
    const line = lines[map.get(key)];
    line.text = replace ? text : line.text + text;
  }

  function reconcileConfig(options, number) {
    if (Array.isArray(options) && number >= configSeq) {
      configOptions = structuredClone(options);
      configSeq = number;
    }
  }

  function apply(event, snapshot = false) {
    const number = event._meta?.hive?.seq;
    if (!snapshot && (!Number.isSafeInteger(number) || number <= seq)) return false;
    if (!snapshot && seq && number !== seq + 1) { gap = true; return false; }
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
    } else if (kind === 'config_option_update') {
      reconcileConfig(update.configOptions, number);
    } else if (kind === '_hive_error' || kind === '_hive_opencode_error') {
      lines.push({ kind: 'notice', text: `${update.message || 'Agent failed'} — ${update.help || 'Check the agent provider credentials, model configuration and logs'}` });
    } else if (kind === 'state_update') {
      if (update.state === 'running') running = true;
      if (update.state === 'idle') running = false;
    } else if (kind === '_hive_control') {
      generation = update.generation;
      pilot = update.pilot;
    } else if (kind === '_hive_activity' && number > activity.seq) {
      if (update.kind === 'foreground') activity.foreground = update.state?.state || 'unknown';
      if (update.kind === 'session') activity.session = update.status;
      for (const [type, id, value] of [['tools', update.toolCallId, update.tool],
        ['plans', update.planId, update.plan], ['tasks', update.taskId, update.task]]) {
        if (id && value) { activity[type][id] = value; activity.support[type] = 'observed'; }
      }
      activity.seq = number;
    } else if (kind === '_hive_usage' && number > usageSeq && update.session) {
      usage = structuredClone(update.session);
      usageSeq = number;
    }
    if (!snapshot) seq = number;
    return true;
  }

  return {
    apply,
    restore(events, through) {
      // Snapshot entries are full upserts, not a contiguous subset of the live event log.
      lines.length = 0; messages.clear(); tools.clear();
      for (const event of events) apply(event, true);
      seq = through; gap = false;
    },
    notice(text) { lines.push({ kind: 'notice', text }); },
    skip(nextSeq) { seq = Math.max(seq, nextSeq - 1); gap = false; },
    reconcile,
    reconcileUsage,
    reconcileConfig,
    get configOptions() { return structuredClone(configOptions); },
    running(value) { running = value; },
    get seq() { return seq; },
    get runningNow() { return running; },
    get generation() { return generation; },
    get pilot() { return pilot; },
    get lines() { return lines.map(line => ({ ...line })); },
    get activity() { return structuredClone(activity); },
    get usage() { return structuredClone(usage); },
    get gap() { return gap; },
  };
}

// Only the common pattern subset is advertised as renderable by all Hive clients.
export function schemaSupported(schema) {
  const choicesSupported = field => (field.enum == null || (Array.isArray(field.enum) && field.enum.every(value => typeof value === 'string'))) &&
    ['oneOf', 'anyOf'].every(kind => field[kind] == null || (Array.isArray(field[kind]) && field[kind].every(option => typeof option?.const === 'string')));
  return Boolean(schema?.properties && typeof schema.properties === 'object' && !Array.isArray(schema.properties) &&
    (schema.type == null || schema.type === 'object') &&
    (schema.required == null || Array.isArray(schema.required)) && Object.entries(schema.properties).every(([, field]) => field && typeof field === 'object' &&
    ['string', 'integer', 'number', 'boolean', 'array'].includes(field.type) &&
    choicesSupported(field) &&
    ['minLength', 'maxLength', 'minItems', 'maxItems'].every(key => field[key] == null || (Number.isSafeInteger(field[key]) && field[key] >= 0)) &&
    ['minimum', 'maximum'].every(key => field[key] == null || (typeof field[key] === 'number' && Number.isFinite(field[key]))) &&
    (field.pattern == null || (typeof field.pattern === 'string' && /^[\x00-\x7f]*$/.test(field.pattern) && !['\\', '.', '(?', '&&', '--', '~~', '[[', '[:'].some(syntax => field.pattern.includes(syntax)) && (() => { try { new RegExp(field.pattern, 'u'); return true; } catch { return false; } })())) &&
    (field.type !== 'array' || (field.items && choicesSupported(field.items) && (field.items.type === 'string' || (field.items.type == null && Array.isArray(field.items.anyOf)))))) &&
    (schema.required || []).every(key => typeof key === 'string' && Object.hasOwn(schema.properties, key)));
}

export function validateContent(schema, content) {
  if (!schemaSupported(schema)) throw new Error('Unsupported form schema; decline this request');
  for (const key of schema.required || []) if (!Object.hasOwn(content, key)) throw new Error(`Required field: ${key}`);
  const choices = (field, value) => (!field.enum || field.enum.includes(value)) &&
    ['oneOf', 'anyOf'].every(kind => !field[kind] || field[kind].some(option => option.const === value));
  for (const [key, value] of Object.entries(content)) {
    const field = schema.properties[key];
    if (!field || !(field.type === 'array' ? Array.isArray(value) && value.every(item => typeof item === 'string')
      : field.type === 'integer' ? Number.isSafeInteger(value)
      : field.type === 'number' ? typeof value === 'number' && Number.isFinite(value)
      : typeof value === field.type)) throw new Error(`${key}: value has the wrong type`);
    if (!choices(field, value)) throw new Error(`${key}: select a listed choice`);
    if (typeof value === 'number' && ((field.minimum != null && value < field.minimum) || (field.maximum != null && value > field.maximum))) throw new Error(`${key}: value is outside the requested bounds`);
    if (typeof value === 'string' && ((field.minLength != null && [...value].length < field.minLength) ||
      (field.maxLength != null && [...value].length > field.maxLength) || (field.pattern != null && (/[\r\n\u2028\u2029]/u.test(value) || !new RegExp(field.pattern, 'u').test(value))))) throw new Error(`${key}: text does not satisfy the requested constraints`);
    if (Array.isArray(value) && ((field.minItems != null && value.length < field.minItems) ||
      (field.maxItems != null && value.length > field.maxItems) || value.some(item => !choices(field.items, item)))) throw new Error(`${key}: select the requested items`);
  }
  return content;
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
  let interactionQueue = [];
  let earlyInteractions = [];
  const urlFlows = new Map();
  const earlyCompletions = new Map();
  const flowTimers = new Map();
  let connectionEpoch = 0;
  let takingControl;
  let gap = false;
  let attaching = false;
  let queuedUpdates = [];
  let status = 'Connecting…';
  let uncertain;
  let recoveryRequired = false;
  let creationUncertain = store?.getItem('hive.creating') === 'true';
  let openQueue = Promise.resolve();
  let openVersion = 0;
  let desired;
  let readyNow = false;
  const readiness = new Set();
  let configuring = false;
  let creating = false;
  let firstResolve;
  const initialized = new Promise(resolve => { firstResolve = resolve; });

  const snapshot = () => ({ connected, status, session: selected && { ...selected }, role: owner?.role || 'observer',
    generation: owner?.generation,
    running: history.runningNow, lines: history.lines, activity: history.activity, usage: history.usage, configOptions: history.configOptions,
    configuring, creating, opening: Boolean(desired && desired.sessionId !== selected?.sessionId), pendingSession: desired?.sessionId, seq: history.seq, gap, interaction,
    urlFlows: [...urlFlows.values()], uncertain: uncertain && { ...uncertain }, recoveryRequired, takingControl: Boolean(takingControl) });
  const publish = () => onChange(snapshot());
  const errorText = error => typeof error?.data === 'string' ? error.data
    : error?.data?.message ? [error.data.message, error.data.help].filter(Boolean).join(' — ')
    : error?.message || String(error);
  function ready() {
    if (readyNow) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const waiter = { resolve() { clearTimeout(timer); readiness.delete(waiter); resolve(); },
        reject(error) { clearTimeout(timer); readiness.delete(waiter); reject(error); } };
      // A rejected handshake must not leave clicks queued forever; a reconnect may retry the latest selection.
      const timer = setTimeout(() => waiter.reject(new Error('ACP connection unavailable; check the daemon URL and connection status')), 10000);
      readiness.add(waiter);
    });
  }
  function request(method, params) {
    if (!connected || socket.readyState !== WebSocket.OPEN) return Promise.reject(new Error('Hive disconnected'));
    const id = ++nextId;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
    });
  }
  function clearInteraction() { interaction = interactionQueue.shift(); publish(); }
  function clearInteractions() {
    if (connected) for (const item of [interaction, ...interactionQueue, ...earlyInteractions].filter(Boolean)) socket.send(JSON.stringify({ jsonrpc: '2.0', id: item.id,
      result: item.method === 'elicitation/create' ? { action: 'cancel' } : { outcome: { outcome: 'cancelled' } } }));
    interaction = undefined; interactionQueue = []; earlyInteractions = [];
  }
  function relinquished() {
    owner = undefined;
    // A request-scoped question belongs to its live parent RPC, independent of pilot ownership.
    const held = [interaction, ...interactionQueue].filter(item => item?.requestScoped);
    interaction = held.shift();
    interactionQueue = held;
    earlyInteractions = [];
    status = `Read-only ${selected?.sessionId || ''} — control transferred`;
    publish();
  }
  function update(message) {
    if (message.params?.sessionId !== selected?.sessionId) return;
    if (attaching) { queuedUpdates.push(message); return; }
    // Only a successful resume may commit its staged transcript snapshot.
    if (message.params._meta?.hive?.snapshot) return;
    const applied = history.apply({ ...message.params, _meta: message.params._meta });
    if (history.gap && !gap) {
      status = 'Activity gap detected — reconciling…';
      publish();
      queueMicrotask(() => resume(owner?.role === 'pilot' ? 'pilot' : 'observe').catch(() => {}));
      return;
    }
    if (!applied) return;
    if (message.params.update?.sessionUpdate === '_hive_activity' && message.params.update.kind === 'session' && message.params.update.status === 'ended') { urlFlows.clear(); clearInteractions(); }
    if (owner?.role === 'pilot' && history.generation > owner.generation) relinquished();
    else publish();
  }
  function inbound(message) {
    if (message.method !== 'session/request_permission' && message.method !== 'elicitation/create') {
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: message.id, error: { code: -32601, message: 'Unsupported ACP interaction' } }));
      return;
    }
    const cancel = () => socket.send(JSON.stringify({ jsonrpc: '2.0', id: message.id, result: message.method === 'elicitation/create' ? { action: 'cancel' } : { outcome: { outcome: 'cancelled' } } }));
    const requestScoped = message.method === 'elicitation/create' && message.params?.requestId != null && pending.has(message.params.requestId);
    if (!connected || (!requestScoped && (!selected || message.params?.sessionId !== selected.sessionId))) { cancel(); return; }
    if (!requestScoped && takingControl && owner?.role !== 'pilot') { earlyInteractions.push(message); return; }
    if (!requestScoped && owner?.role !== 'pilot') { cancel(); return; }
    if (message.method === 'elicitation/create' && (message.params.mode === 'form' ? !schemaSupported(message.params.requestedSchema) : message.params.mode !== 'url')) { cancel(); return; }
    if ([interaction, ...interactionQueue].some(item => item?.id === message.id)) return;
    if (interactionQueue.length >= 128) { cancel(); return; }
    if (message.params?.mode === 'url') {
      try {
        if (!['http:', 'https:'].includes(new URL(message.params.url).protocol)) { cancel(); return; }
      } catch { cancel(); return; }
      if (urlFlows.size >= 128) {
        const oldest = urlFlows.keys().next().value;
        urlFlows.delete(oldest);
        clearTimeout(flowTimers.get(oldest));
        flowTimers.delete(oldest);
      }
      const complete = earlyCompletions.get(message.params.elicitationId) > Date.now();
      earlyCompletions.delete(message.params.elicitationId);
      urlFlows.set(message.params.elicitationId, { id: message.params.elicitationId, message: message.params.message, complete, expires: Date.now() + 120000 });
      const timer = setTimeout(() => { flowTimers.delete(message.params.elicitationId); publish(); }, 120000);
      timer.unref?.();
      flowTimers.set(message.params.elicitationId, timer);
    }
    // A new pilot receives unanswered requests again; only the current generation may respond.
    const next = { id: message.id, method: message.method, params: message.params, generation: owner?.generation, requestScoped, epoch: connectionEpoch };
    if (interaction) interactionQueue.push(next); else interaction = next;
    publish();
  }
  function handle(message) {
    // Request IDs are independent in each direction; a request is never a response.
    if (message.method) {
      if (message.id != null) inbound(message);
      else if (message.method === 'session/update') update(message);
      else if (message.method === '$/cancel_request') {
        const id = message.params?.requestId;
        if ([interaction, ...interactionQueue, ...earlyInteractions].some(item => item?.id === id)) socket.send(JSON.stringify({ jsonrpc: '2.0', id, error: { code: -32800, message: 'Request cancelled' } }));
        interactionQueue = interactionQueue.filter(item => item.id !== id);
        earlyInteractions = earlyInteractions.filter(item => item.id !== id);
        if (interaction?.id === id) clearInteraction(); else publish();
      } else if (message.method === 'elicitation/complete') {
        const flow = urlFlows.get(message.params?.elicitationId);
        if (flow) { flow.complete = true; clearTimeout(flowTimers.get(flow.id)); flowTimers.delete(flow.id); publish(); }
        else if (typeof message.params?.elicitationId === 'string') {
          if (earlyCompletions.size >= 128) earlyCompletions.delete(earlyCompletions.keys().next().value);
          earlyCompletions.set(message.params.elicitationId, Date.now() + 120000);
        }
      }
      return;
    }
    if (message.id != null && ('result' in message || 'error' in message) && pending.has(message.id)) {
      const task = pending.get(message.id);
      pending.delete(message.id);
      const abandoned = [interaction, ...interactionQueue].filter(item => item?.requestScoped && item.params.requestId === message.id);
      for (const item of abandoned) {
        socket.send(JSON.stringify({ jsonrpc: '2.0', id: item.id, result: { action: 'cancel' } }));
        interactionQueue = interactionQueue.filter(queued => queued.id !== item.id);
        if (interaction?.id === item.id) clearInteraction();
      }
      if (message.error) task.reject(new Error(errorText(message.error))); else task.resolve(message.result);
    }
  }
  function ownership(meta) {
    const hive = meta?._meta?.hive;
    if (!hive || !Number.isSafeInteger(hive.generation) || !hive.role) throw new Error('Hive did not return control metadata');
    owner = hive.role === 'pilot' ? hive : { generation: hive.generation, role: 'observer' };
    recoveryRequired = Boolean(hive.recoveryRequired);
    history.running(hive.running);
    gap = false;
    status = `${owner.role === 'pilot' ? 'Controlling' : 'Read-only'} ${selected.sessionId}${hive.running ? ' (running)' : ''}`;
    publish();
    const early = earlyInteractions;
    earlyInteractions = [];
    if (owner.role === 'pilot') for (const message of early) inbound(message);
    return hive;
  }
  async function resume(mode = 'observe', replay = true, acknowledge = false) {
    if (!selected) throw new Error('Select a session first');
    const target = selected.sessionId;
    const targetCwd = selected.cwd;
    const version = openVersion;
    const params = { sessionId: selected.sessionId, cwd: selected.cwd,
      ...((mode === 'observe' || acknowledge) ? { _meta: { hive: { ...(mode === 'observe' ? { mode } : {}),
        ...(acknowledge ? { recoveryAction: 'acknowledge' } : {}) } } } : {}) };
    if (replay) params.replayFrom = history.seq
      ? { type: '_hive_seq', seq: history.seq + 1 } : { type: 'start' };
    attaching = true;
    try {
      const response = await request('session/resume', params);
      // A delayed response from an older attachment must never grant control over the new view.
      if (selected?.sessionId !== target || version !== openVersion) {
        attaching = false;
        queuedUpdates = [];
        const stale = response?._meta?.hive;
        if (stale?.role === 'pilot' && stale.token) {
          // An overtaken handoff may succeed at the daemon after the user has left its view.
          await request('session/resume', { sessionId: target, cwd: targetCwd,
            _meta: { hive: { mode: 'relinquish', generation: stale.generation, token: stale.token } } });
        }
        return response;
      }
      const hive = ownership(response);
      if (Number.isSafeInteger(hive.snapshotThroughSeq)) {
        history.restore(queuedUpdates.filter(message => message.params._meta?.hive?.snapshot).map(message => message.params), hive.snapshotThroughSeq);
        queuedUpdates = queuedUpdates.filter(message => !message.params._meta?.hive?.snapshot);
      }
      history.reconcile(hive.activitySnapshot);
      history.reconcileUsage(hive.usageSnapshot, hive.usageSeq);
      history.reconcileConfig(response.configOptions || [], hive.configSeq || 0);
      if (!replay) {
        // With no retained events, move the cursor to the daemon's atomic attachment boundary.
        history.skip(hive.nextSeq);
        history.notice('[Earlier history unavailable]');
        publish();
      }
      attaching = false;
      for (const queued of queuedUpdates.splice(0)) update(queued);
      publish();
      return response;
    } catch (error) {
      attaching = false;
      queuedUpdates = [];
      if (selected?.sessionId !== target || version !== openVersion) throw error;
      if (error.message.includes('replay cursor unavailable')) {
        gap = true;
        status = 'History expired — attach without earlier events to continue';
      } else if (error.message.includes('hive::transcript::unsupported') || error.message.includes('hive::transcript::history_missing')) {
        gap = true;
        status = error.message;
      } else status = error.message;
      publish();
      throw error;
    }
  }
  function openSocket() {
    if (closed) return;
    const ws = new WebSocket(url);
    socket = ws;
    connectionEpoch++;
    urlFlows.clear();
    earlyCompletions.clear();
    for (const timer of flowTimers.values()) clearTimeout(timer);
    flowTimers.clear();
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
        readyNow = true;
        const waiting = readiness.size > 0;
        for (const waiter of [...readiness]) waiter.resolve();
        firstResolve(response);
        if (desired && desired.sessionId !== selected?.sessionId && !waiting) await client.open(desired);
        else if (selected && !waiting) await resume('observe');
        else {
          status = creationUncertain ? 'Session creation interrupted — select the original session from the list'
            : desired ? `Opening ${desired.sessionId}…` : 'Connected to ACPv2';
          publish();
        }
      } catch (error) {
        status = error.message;
        publish();
        // An expired replay needs an explicit choice, but the transport is usable.
        if (!readyNow && ws.readyState === WebSocket.OPEN) ws.close();
      }
    };
    ws.onclose = () => {
      if (socket !== ws) return;
      connected = false;
      readyNow = false;
      for (const waiter of [...readiness]) waiter.reject(new Error('ACP connection failed; check the daemon URL. Reconnecting…'));
      attaching = false;
      queuedUpdates = [];
      owner = undefined;
      clearInteractions();
      for (const timer of flowTimers.values()) clearTimeout(timer);
      flowTimers.clear();
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

  const client = {
    initialized,
    state: snapshot,
    open(session) {
      if (!session?.sessionId || !session.cwd) throw new Error('Select a listed Hive session with a cwd');
      desired = { ...session };
      status = `Opening ${session.sessionId}${readyNow ? '' : ' — waiting for ACP connection'}…`;
      publish();
      const version = ++openVersion;
      const operation = openQueue.then(async () => {
        await ready();
        if (version !== openVersion) return;
        if (selected?.sessionId === session.sessionId && owner) {
          status = `${owner.role === 'pilot' ? 'Controlling' : 'Read-only'} ${session.sessionId}`;
          publish();
          return;
        }
        if (selected && owner?.role === 'pilot' && selected.sessionId !== session.sessionId) {
          // Leaving an owned conversation must release its pilot token without ending the agent session.
          await request('session/resume', { sessionId: selected.sessionId, cwd: selected.cwd,
            _meta: { hive: { mode: 'relinquish', generation: owner.generation, token: owner.token } } });
        }
        if (version !== openVersion) return;
        selected = { sessionId: session.sessionId, cwd: session.cwd, agent: session.agent };
        recoveryRequired = session.recovery === 'recovery_required';
        creationUncertain = false;
        store?.removeItem('hive.creating');
        history = conversation();
        owner = undefined;
        clearInteractions();
        uncertain = JSON.parse(store?.getItem(`hive.pending.${selected.sessionId}`) || 'null');
        publish();
        return resume('observe');
      });
      openQueue = operation.catch(() => {});
      return operation;
    },
    async attachWithoutHistory() { if (!gap) throw new Error('No history gap to skip'); return resume('observe', false); },
    async acknowledgeRecovery() {
      if (!selected || !connected || gap || !recoveryRequired) throw new Error('Observe the recovered session before acknowledging interrupted work');
      const response = await resume('pilot', true, true);
      uncertain = undefined;
      store?.removeItem(`hive.pending.${selected.sessionId}`);
      history.running(false);
      publish();
      return response;
    },
    async takeControl() {
      if (!selected || !connected || gap) throw new Error('Attach to a session before taking control');
      if (owner?.role === 'pilot') return;
      if (!takingControl) {
        takingControl = resume('pilot').finally(() => { takingControl = undefined; publish(); });
        publish();
      }
      return takingControl;
    },
    async create(agent = '') {
      await ready();
      if (creating || configuring) throw new Error('Wait for session configuration to finish');
      if (selected) throw new Error('A session is already selected');
      if (creationUncertain) throw new Error('Session creation was interrupted; choose the original session from the list before prompting');
      creating = true;
      const version = openVersion;
      try {
        creationUncertain = true;
        store?.setItem('hive.creating', 'true');
        publish();
        let created;
        try {
          created = await request('session/new', { cwd: '/', mcpServers: [],
            ...(agent ? { _meta: { hive: { agent } } } : {}) });
        } catch (error) {
          if (connected) { creationUncertain = false; store?.removeItem('hive.creating'); }
          throw error;
        }
        if (version !== openVersion) {
          // Selection may change while creation asks a question; don't leave an invisible pilot behind.
          const hive = created._meta?.hive;
          if (hive?.role === 'pilot') await request('session/resume', { sessionId: created.sessionId, cwd: '/',
            _meta: { hive: { mode: 'relinquish', generation: hive.generation, token: hive.token } } });
          creationUncertain = false;
          store?.removeItem('hive.creating');
          throw new Error('Session created; select it from the session list');
        }
        selected = { sessionId: created.sessionId, cwd: '/', agent };
        desired = { ...selected };
        creationUncertain = false;
        store?.removeItem('hive.creating');
        history = conversation();
        const hive = ownership(created);
        history.reconcileUsage(hive.usageSnapshot, hive.usageSeq);
        history.reconcileConfig(created.configOptions || [], hive.configSeq || 0);
        return created;
      } finally { creating = false; publish(); }
    },
    async configure(configId, value) {
      await ready();
      if (!selected || (desired && desired.sessionId !== selected.sessionId) || owner?.role !== 'pilot' || history.runningNow || configuring || creating || recoveryRequired || uncertain) throw new Error('Take control of an idle session before changing its model');
      const target = selected.sessionId;
      const version = openVersion;
      const epoch = connectionEpoch;
      configuring = true;
      publish();
      try {
        const response = await request('session/set_config_option', { sessionId: target, configId, type: 'id', value,
          _meta: { hive: { generation: owner.generation, token: owner.token } } });
        if (selected?.sessionId === target && version === openVersion && epoch === connectionEpoch) {
          history.reconcileConfig(response.configOptions || [], response._meta?.hive?.configSeq ?? history.seq);
          status = 'Session configuration updated';
        }
        return response;
      } catch (error) {
        if (selected?.sessionId === target && version === openVersion && epoch === connectionEpoch) status = error.message;
        throw error;
      }
      finally { configuring = false; publish(); }
    },
    async prompt(text, agent = '') {
      await ready();
      if (!text?.trim()) throw new Error('Enter a prompt');
      if (!selected) await client.create(agent);
      if (configuring || creating) throw new Error('Wait for session configuration to finish');
      if (desired && desired.sessionId !== selected.sessionId) throw new Error('Wait for the selected session to open');
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
        if (error.message.includes('outcome is uncertain after restart')) {
          recoveryRequired = true;
          status = `${error.message}; inspect agent state and acknowledge recovery`;
        } else status = `Prompt acknowledgement uncertain: ${error.message}; retry with the same key`;
        if (error.message.includes('stale')) relinquished();
        publish();
        throw error;
      }
    },
    cancel() {
      if (!connected || owner?.role !== 'pilot') return false;
      socket.send(JSON.stringify({ jsonrpc: '2.0', method: 'session/cancel', params: { sessionId: selected.sessionId,
        _meta: { hive: { generation: owner.generation, token: owner.token } } } }));
      urlFlows.clear();
      while (interaction) this.respond(interaction.method === 'elicitation/create'
        ? { action: 'cancel' } : { outcome: { outcome: 'cancelled' } }, interaction.id);
      return true;
    },
    respond(result, id) {
      if (!connected || !interaction || interaction.id !== id || interaction.epoch !== connectionEpoch ||
          (!interaction.requestScoped && (owner?.role !== 'pilot' || interaction.generation !== owner.generation))) return false;
      if (interaction.method === 'session/request_permission' && result.outcome?.outcome === 'selected' &&
          !interaction.params.options?.some(option => option.optionId === result.outcome.optionId)) return false;
      if (interaction.method === 'session/request_permission' && !['selected', 'cancelled'].includes(result.outcome?.outcome)) return false;
      if (interaction.method === 'elicitation/create' && !['accept', 'decline', 'cancel'].includes(result.action)) return false;
      if (interaction.params.mode === 'url' && result.action !== 'accept') {
        urlFlows.delete(interaction.params.elicitationId);
        clearTimeout(flowTimers.get(interaction.params.elicitationId));
        flowTimers.delete(interaction.params.elicitationId);
      }
      if (interaction.method === 'elicitation/create' && result.action === 'accept') {
        if (interaction.params.mode === 'form') validateContent(interaction.params.requestedSchema, result.content);
        else if (interaction.params.mode === 'url') {
          const url = new URL(interaction.params.url);
          if (!['http:', 'https:'].includes(url.protocol)) return false;
          const previous = urlFlows.get(interaction.params.elicitationId);
          urlFlows.set(interaction.params.elicitationId, { id: interaction.params.elicitationId, message: interaction.params.message, complete: previous?.complete || false, expires: previous?.expires || Date.now() + 120000 });
        } else return false;
      }
      socket.send(JSON.stringify({ jsonrpc: '2.0', id, result }));
      clearInteraction();
      return true;
    },
    close() { closed = true; clearTimeout(retryTimer); socket?.close(); },
  };
  return client;
}

// Catalog revisions are invalidations, never conversation updates or browser ownership state.
export function catalog(onChange, onError = () => {}) {
  let revision = -1;
  let stopped = false;
  let requestId = 0;
  const events = new EventSource('/v1/sessions/events');
  async function refresh() {
    const id = ++requestId;
    try {
      const response = await fetch('/v1/sessions');
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
  // Agent-provided property names such as __proto__ must remain ordinary form keys.
  const content = Object.create(null);
  for (const [name, field] of Object.entries(schema?.properties || {})) {
    const input = form.elements.namedItem(name);
    if (field.type === 'array' && input.multiple) {
      const values = Array.from(input.selectedOptions, option => option.value);
      if (values.length || schema.required?.includes(name)) content[name] = values;
    } else if (field.type === 'boolean') {
      if (input.value !== '') content[name] = input.value === 'true';
    } else if (input.value !== '' || input.dataset.includeEmpty === 'true' || (field.type === 'string' && schema.required?.includes(name))) {
      if (field.type === 'integer' || field.type === 'number') {
        const number = Number(input.value);
        if (!Number.isFinite(number) || (field.type === 'integer' && !Number.isSafeInteger(number))) throw new Error(`${name} needs a ${field.type}`);
        content[name] = number;
      } else if (field.type === 'array') content[name] = input.value.startsWith('[') ? JSON.parse(input.value) : input.value.split(',').map(part => part.trim());
      else content[name] = input.value;
    }
  }
  return validateContent(schema, content);
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
  const recover = root.querySelector('#recover');
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
    const create = root.querySelector('#create');
    if (create) {
      create.hidden = Boolean(state.session);
      create.disabled = !state.connected || state.creating || state.opening;
      create.onclick = () => client.create(agentSelect.value).catch(error => { status.textContent = error.message; });
    }
    const control = root.querySelector('#control');
    if (control) {
      control.hidden = !state.session || pilot;
      control.disabled = !state.connected || state.recoveryRequired || state.takingControl;
      control.onclick = () => client.takeControl().catch(error => { status.textContent = error.message; });
    }
    const configuration = root.querySelector('#configuration');
    if (configuration) {
      const focused = root.activeElement?.dataset?.configId;
      configuration.replaceChildren();
      const options = (state.configOptions || []).filter(option => option.type === 'select');
      if (!options.length) add(configuration, 'p', state.session ? 'This backend does not expose model selection' : 'Create a session to choose its provider / model before sending a prompt');
      for (const option of options) {
        const label = add(configuration, 'label', option.category === 'model' ? 'Provider / Model ' : `${option.name} `);
        const select = add(label, 'select', '', { 'aria-label': option.category === 'model' ? 'Provider / Model' : option.name });
        select.dataset.configId = option.configId;
        for (const choice of option.options || []) {
          if (Array.isArray(choice.options)) {
            const group = add(select, 'optgroup', '', { label: choice.name || choice.group });
            for (const item of choice.options) add(group, 'option', item.name, { value: item.value });
          } else add(select, 'option', choice.name, { value: choice.value });
        }
        select.value = option.currentValue;
        select.disabled = !pilot || state.opening || state.running || state.configuring || state.creating || state.recoveryRequired || Boolean(state.uncertain);
        select.onchange = () => client.configure(option.configId, select.value).catch(error => { status.textContent = error.message; });
        if (focused === option.configId) select.focus();
      }
    }
    input.disabled = !state.connected || state.gap || state.opening;
    send.disabled = !state.connected || state.gap || sending || state.takingControl || state.running || state.recoveryRequired ||
      state.opening || state.configuring || state.creating || (!state.session && Boolean(create)) || (Boolean(state.session) && !pilot) || Boolean(state.uncertain);
    cancel.disabled = !pilot || !state.running;
    retry.hidden = !state.uncertain || state.recoveryRequired;
    retry.disabled = !pilot || !state.connected || sending;
    if (recover) {
      recover.hidden = !state.recoveryRequired;
      recover.disabled = !state.connected || state.gap || sending;
    }
    skip.hidden = !state.gap;
    agentSelect.disabled = Boolean(state.session);
    if (state.session?.agent && Array.from(agentSelect.options).some(option => option.value === state.session.agent)) {
      agentSelect.value = state.session.agent;
    }
    output.replaceChildren();
    for (const line of state.lines) add(output, 'div', `${line.kind === 'notice' ? '' : `${line.kind}: `}${line.text}`);
    const usage = state.usage || {};
    const actual = usage.actualSessionCost;
    const estimated = usage.estimatedCost;
    const price = actual?.amount != null ? `actual ${actual.amount.toFixed(4)} ${actual.currency}`
      : estimated?.amount != null ? `estimated ${estimated.amount.toFixed(4)} ${estimated.currency}` : 'unknown';
    const usagePanel = add(output, 'section', '');
    add(usagePanel, 'h3', 'Usage');
    add(usagePanel, 'div', `Tokens: ${usage.tokens?.totalTokens ?? 'unknown'} · Cost: ${price}`);
    add(usagePanel, 'div', `Reported turns: ${usage.reportedTurns ?? 0} / ${usage.turns ?? 0}`);
    const activity = state.activity;
    if (activity) {
      const panel = add(output, 'section', '');
      add(panel, 'h3', `Activity · ${activity.session || 'active'} / ${activity.foreground}`);
      for (const [label, key] of [['Tools', 'tools'], ['Tasks', 'tasks'], ['Todos', 'plans']]) {
        add(panel, 'h4', `${label} · ${activity.support?.[key] || 'unknown'}`);
        for (const [id, item] of Object.entries(activity[key] || {})) {
          if (key === 'plans') {
            for (const entry of item.entries || []) add(panel, 'div', `${entry.status}: ${entry.content}`);
          } else add(panel, 'div', `${item.title || id} · ${item.status || 'unknown'}`);
        }
      }
    }
    for (const flow of state.urlFlows || []) add(output, 'p', `${flow.message}: ${flow.complete ? 'Completed' : flow.expires <= Date.now() ? 'Completion not received before timeout' : 'Waiting for external completion'}`);
    if (currentInteraction?.id === state.interaction?.id && currentInteraction?.epoch === state.interaction?.epoch && currentInteraction?.generation === state.interaction?.generation) return;
    currentInteraction = state.interaction;
    dialog.replaceChildren();
    dialog.hidden = !state.interaction || (!pilot && !state.interaction.requestScoped);
    if (dialog.hidden) return;
    const interaction = state.interaction;
    const params = interaction.params;
    add(dialog, 'p', params.title || params.message || 'Agent needs input');
    if (params.description) add(dialog, 'p', params.description);
    if (params.subject) add(dialog, 'pre', JSON.stringify(params.subject, null, 2));
    const form = add(dialog, 'form', '');
    if (interaction.method === 'session/request_permission') {
      for (const option of params.options || []) {
        const button = add(form, 'button', `${option.name}${option.kind ? ` (${option.kind})` : ''}`, { type: 'button' });
        button.onclick = () => client.respond({ outcome: { outcome: 'selected', optionId: option.optionId } }, interaction.id);
      }
    } else if (params.mode === 'form') {
      if (params.requestedSchema.title) add(form, 'h3', params.requestedSchema.title);
      if (params.requestedSchema.description) add(form, 'p', params.requestedSchema.description);
      for (const [name, field] of Object.entries(params.requestedSchema?.properties || {})) {
        const label = add(form, 'label', `${field.title || name}${params.requestedSchema.required?.includes(name) ? ' (required)' : ''}`);
        const options = field.enum?.map(value => ({ const: value, title: value })) || field.oneOf || field.items?.enum?.map(value => ({ const: value, title: value })) || field.items?.anyOf;
        let control;
        if (options) {
          control = add(label, 'select', '', { name });
          if (field.type === 'array') control.multiple = true;
          else add(control, 'option', 'Choose…', { value: '' });
          for (const option of options) add(control, 'option', option.title || option.const, { value: option.const });
        } else control = add(label, 'input', '', { name, type: field.type === 'integer' || field.type === 'number' ? 'number' : 'text' });
        if (field.type === 'boolean') {
          const select = add(label, 'select', '', { name });
          control.remove();
          for (const value of ['', 'true', 'false']) add(select, 'option', value || 'Choose…', { value });
          control = select;
        }
        if (field.description) add(label, 'small', field.description);
        const constraints = add(form, 'details', '');
        add(constraints, 'summary', `Constraints for ${field.title || name}`);
        add(constraints, 'pre', JSON.stringify(field, null, 2));
        if (field.type === 'string' && !params.requestedSchema.required?.includes(name)) {
          const empty = add(label, 'button', 'Include empty value', { type: 'button' });
          empty.onclick = () => { control.dataset.includeEmpty = control.dataset.includeEmpty === 'true' ? 'false' : 'true'; empty.textContent = control.dataset.includeEmpty === 'true' ? 'Omit empty value' : 'Include empty value'; };
        }
        if (field.default != null) {
          if (control.multiple) for (const option of control.options) option.selected = field.default.includes(option.value);
          else control.value = String(field.default);
        }
        if (field.type === 'number') control.step = 'any';
        control.setAttribute('aria-required', String(Boolean(params.requestedSchema.required?.includes(name))));
        if (field.type === 'integer') control.step = '1';
        if (field.minimum != null) control.min = field.minimum;
        if (field.maximum != null) control.max = field.maximum;
      }
      const errors = add(form, 'p', '', { role: 'alert' });
      const accept = add(form, 'button', 'Submit', { type: 'submit' });
      accept.disabled = false;
      form.onsubmit = event => {
        event.preventDefault();
        try { client.respond({ action: 'accept', content: formValues(params.requestedSchema, form) }, interaction.id); }
        catch (error) { errors.textContent = error.message; }
      };
    } else if (params.mode === 'url') {
      try {
        const url = new URL(params.url);
        if (url.protocol === 'http:' || url.protocol === 'https:') add(form, 'a', url.href, { href: url.href, target: '_blank', rel: 'noopener noreferrer' });
      } catch { /* An invalid agent URL must not become an unsafe browser link. */ }
      add(form, 'p', 'Continue to allow the external flow; Hive will report completion when the agent notifies it.');
      add(form, 'button', 'Continue', { type: 'button' }).onclick = () => client.respond({ action: 'accept' }, interaction.id);
    }
    add(form, 'button', 'Decline', { type: 'button' }).onclick = () => client.respond(
      interaction.method === 'session/request_permission' ? { outcome: { outcome: 'cancelled' } } : { action: 'decline' }, interaction.id);
    form.querySelector('input, select, button, a')?.focus();
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
      const detail = add(button, 'small', `${session.sessionId} · ${session.agent || 'unknown agent'} · ${activity || 'unknown'}${session.recoveryReason ? ` · ${session.recoveryReason}` : ''}${attention} · ${control} · ` +
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
  fetch('/v1/providers').then(response => response.json()).then(({ agents }) => {
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
  if (recover) recover.onclick = () => client.acknowledgeRecovery().catch(error => { status.textContent = error.message; });
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
