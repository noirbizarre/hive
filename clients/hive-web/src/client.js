// The daemon owns lifecycle and control; this adapter projects transcript snapshots and live events.
export function conversation() {
  const lines = [];
  const messages = new Map();
  const tools = new Map();
  const toolStates = new Map();
  let seq = 0;
  let running = false;
  let generation = 0;
  let pilot = false;
  let activity = { version: 1, seq: 0, session: 'active', foreground: 'unknown', tools: {}, plans: {}, tasks: {},
    support: { tools: 'unknown', plans: 'unknown', tasks: 'unknown' } };
  let usage = {};
  let planSnapshot = { version: 1, seq: 0, support: 'unknown', observation: 'not_observed', plans: {} };
  let plansAuthoritative = false;
  let usageSeq = 0;
  let configOptions = [];
  let configSeq = 0;
  let transcript;
  let transcriptSeq = 0;
  let gap = false;
  let activeThought;

  function foreground(state) {
    activity.foreground = ['running', 'requires_action', 'idle'].includes(state) ? state : 'unknown';
    // Blocked work owns the foreground without implying that the agent is executing.
    running = ['running', 'requires_action'].includes(activity.foreground);
  }

  function finishThought() {
    if (activeThought) activeThought.active = false;
    activeThought = undefined;
  }

  function reconcile(snapshot) {
    if (snapshot?.version === 1 && snapshot.seq >= activity.seq) {
      activity = structuredClone(snapshot);
      foreground(snapshot.foreground);
    }
    gap = false;
  }

  function reconcileUsage(snapshot, number) {
    if (snapshot && Number.isSafeInteger(number) && number >= usageSeq) {
      usage = structuredClone(snapshot);
      usageSeq = number;
    }
  }

  function reconcilePlans(snapshot) {
    // An older attachment cannot roll back plans already observed beyond its boundary.
    if (snapshot?.version === 1 && Number.isSafeInteger(snapshot.seq) && snapshot.seq >= planSnapshot.seq) {
      planSnapshot = structuredClone(snapshot);
      plansAuthoritative = true;
    }
  }

  function upsert(map, key, kind, text, replace = false) {
    if (!map.has(key)) {
      map.set(key, lines.length);
      lines.push({ kind, text: '' });
    }
    const line = lines[map.get(key)];
    line.text = replace ? text : line.text + text;
    return line;
  }

  function reconcileConfig(options, number) {
    if (Array.isArray(options) && number >= configSeq) {
      configOptions = structuredClone(options);
      configSeq = number;
    }
  }

  function reconcileTranscript(value, number) {
    // An old replayed control update must not undo the successful attachment's status.
    if (value && Number.isSafeInteger(number) && number >= transcriptSeq) {
      transcript = structuredClone(value);
      transcriptSeq = number;
    }
  }

  function apply(event, snapshot = false) {
    const number = event._meta?.hive?.seq;
    if (!snapshot && (!Number.isSafeInteger(number) || number <= seq)) return false;
    if (!snapshot && seq && number !== seq + 1) { gap = true; return false; }
    const update = event.update;
    const kind = update.sessionUpdate;
    // Only conversational output closes thinking; usage and other metadata can interleave chunks.
    if ((['agent_message_chunk', 'agent_message', 'tool_call_update', 'tool_call_content_chunk', 'state_update'].includes(kind) || kind.startsWith('_hive_v1_tool')) &&
      (kind !== 'state_update' || update.state === 'idle')) finishThought();
    if (kind === 'agent_message_chunk' || kind === 'user_message_chunk' || kind === 'agent_thought_chunk') {
      const speaker = kind.startsWith('agent_thought') ? 'thought' : kind.startsWith('agent') ? 'agent' : 'user';
      const key = `${speaker}:${update.messageId}`;
      if (speaker === 'thought' && activeThought !== lines[messages.get(key)]) finishThought();
      const line = upsert(messages, key, speaker, update.content?.text || '');
      if (speaker === 'thought' && !snapshot) { line.active = true; activeThought = line; }
    } else if (kind === 'agent_message' || kind === 'user_message' || kind === 'agent_thought') {
      const speaker = kind === 'agent_thought' ? 'thought' : kind.startsWith('agent') ? 'agent' : 'user';
      const line = upsert(messages, `${speaker}:${update.messageId}`, speaker,
        (update.content || []).map(block => block.text ?? `[${block.type || 'content'}]`).join('\n'),
        // Metadata-only patches reserve a slot without erasing streamed text.
        Object.hasOwn(update, 'content'));
      if (speaker === 'thought') { line.active = false; if (activeThought === line) activeThought = undefined; }
    } else if (kind === 'tool_call_update' || kind === 'tool_call_content_chunk' || kind.startsWith('_hive_v1_tool')) {
      const tool = update.tool || update;
      const id = tool.toolCallId || 'tool';
      const stored = toolStates.get(id) || {};
      if (kind === 'tool_call_content_chunk') {
        // A chunk starts a new content array after a clear, just like daemon materialization.
        if (!Array.isArray(stored.content)) stored.content = [];
        stored.content.push(structuredClone(tool.content));
      } else {
        // Keep field presence independent of display defaults and explicit nulls.
        Object.assign(stored, structuredClone(tool));
      }
      toolStates.set(id, stored);
      // Empty/cleared structured content falls back only to the current raw output.
      const content = Array.isArray(stored.content) && stored.content.length ? stored.content : stored.rawOutput;
      const readable = value => typeof value === 'string' ? value
        : Array.isArray(value) ? value.map(block => block.content?.text ?? block.text ?? JSON.stringify(block, null, 2)).join('\n')
        : JSON.stringify(value, null, 2);
      const line = upsert(tools, id, 'tool', content == null ? '' : readable(content), true);
      line.title = stored.title ?? 'Tool activity';
      line.status = stored.status ?? 'unknown';
      if (stored.rawInput != null) line.input = readable(stored.rawInput);
      else delete line.input;
    } else if (kind === 'plan_update' && (snapshot ? number >= planSnapshot.seq : number > planSnapshot.seq) && typeof update.plan?.planId === 'string') {
      // Full snapshot upserts share a boundary; ordinary replay must be strictly newer.
      // ACP itemized updates replace the entire ordered plan, never merge entries by position.
      planSnapshot.plans = { ...planSnapshot.plans, [update.plan.planId]: structuredClone(update) };
      planSnapshot.seq = number;
      planSnapshot.support = 'supported';
      planSnapshot.observation = 'observed';
    } else if (kind === 'config_option_update') {
      reconcileConfig(update.configOptions, number);
    } else if (kind === '_hive_error' || kind === '_hive_opencode_error') {
      lines.push({ kind: 'notice', text: `${update.message || 'Agent failed'} — ${update.help || 'Check the agent provider credentials, model configuration and logs'}` });
    } else if (kind === 'state_update' && number >= activity.seq) {
      // Older replay must not undo current foreground evidence from attachment metadata.
      foreground(update.state);
      activity.seq = number;
    } else if (kind === '_hive_control') {
      generation = update.generation;
      pilot = update.pilot;
      reconcileTranscript(update.transcript, number);
    } else if (kind === '_hive_activity' && number > activity.seq) {
      if (update.kind === 'foreground') foreground(update.state?.state);
      if (update.kind === 'session') activity.session = update.status;
      for (const [type, id, value] of [['tools', update.toolCallId, update.tool],
        ['plans', update.planId, update.plan], ['tasks', update.taskId, update.task]]) {
        // Full entity upserts must drop fields removed by authoritative content replacements.
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
      finishThought(); lines.length = 0; messages.clear(); tools.clear(); toolStates.clear();
      for (const event of events) apply(event, true);
      seq = through; gap = false;
    },
    notice(text) { lines.push({ kind: 'notice', text }); },
    skip(nextSeq) { seq = Math.max(seq, nextSeq - 1); gap = false; },
    reconcile,
    reconcilePlans,
    get planSnapshot() { return { ...structuredClone(planSnapshot), authoritative: plansAuthoritative }; },
    reconcileUsage,
    reconcileConfig,
    reconcileTranscript,
    get transcript() { return transcript && structuredClone(transcript); },
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
  let opening;
  let stopping = false;
  let firstResolve;
  const initialized = new Promise(resolve => { firstResolve = resolve; });

  const snapshot = () => ({ connected, status, session: selected && { ...selected }, role: owner?.role || 'observer',
    generation: owner?.generation,
    running: history.runningNow, lines: history.lines, activity: history.activity, planSnapshot: history.planSnapshot, usage: history.usage, configOptions: history.configOptions, transcript: history.transcript,
    configuring, creating, stopping, opening: Boolean(opening || attaching), pendingSession: desired?.sessionId, seq: history.seq, gap, interaction,
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
    stopping = false;
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
    if (!history.runningNow || history.activity.session === 'ended') stopping = false;
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
    history.reconcileTranscript(hive.transcript, hive.transcriptSeq ?? 0);
    history.reconcilePlans(hive.planSnapshot);
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
    const epoch = connectionEpoch;
    const attachment = Symbol('attachment');
    const params = { sessionId: selected.sessionId, cwd: selected.cwd,
      ...((mode === 'observe' || acknowledge) ? { _meta: { hive: { ...(mode === 'observe' ? { mode } : {}),
        ...(acknowledge ? { recoveryAction: 'acknowledge' } : {}) } } } : {}) };
    if (replay) params.replayFrom = history.seq
      ? { type: '_hive_seq', seq: history.seq + 1 } : { type: 'start' };
    attaching = attachment;
    publish();
    try {
      const response = await request('session/resume', params);
      // A previous socket's promise may settle after reconnect has begun a new attachment.
      if (epoch !== connectionEpoch) return response;
      // A delayed response from an older attachment must never grant control over the new view.
      if (selected?.sessionId !== target || version !== openVersion || attaching !== attachment) {
        if (attaching === attachment) { attaching = false; queuedUpdates = []; }
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
      history.reconcilePlans(hive.planSnapshot);
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
      if (epoch !== connectionEpoch || attaching !== attachment) throw error;
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
      stopping = false;
      opening = undefined;
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
      const version = ++openVersion;
      opening = version;
      status = `Opening ${session.sessionId}${readyNow ? '' : ' — waiting for ACP connection'}…`;
      publish();
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
        stopping = false;
        owner = undefined;
        clearInteractions();
        uncertain = JSON.parse(store?.getItem(`hive.pending.${selected.sessionId}`) || 'null');
        publish();
        return resume('observe');
      });
      const completed = operation.finally(() => { if (opening === version) opening = undefined; publish(); });
      openQueue = completed.catch(() => {});
      return completed;
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
        const operation = resume('pilot').finally(() => { if (takingControl === operation) takingControl = undefined; publish(); });
        takingControl = operation;
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
      if (!selected || (desired && desired.sessionId !== selected.sessionId) || owner?.role !== 'pilot' || history.runningNow || configuring || creating || recoveryRequired || uncertain) throw new Error('Take control of an idle session before changing its configuration');
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
      stopping = history.runningNow;
      publish();
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
export function catalog(onChange, onError = () => {}, onProviders) {
  let revision = -1;
  let stopped = false;
  let requestId = 0;
  let providerRequest = 0;
  let providerRetry;
  const events = new EventSource('/v1/sessions/events');
  async function refreshProviders() {
    if (!onProviders || stopped) return;
    const id = ++providerRequest;
    clearTimeout(providerRetry);
    try {
      const response = await fetch('/v1/providers');
      if (!response.ok) throw new Error(`Agent inventory unavailable (${response.status})`);
      const inventory = await response.json();
      if (!stopped && id === providerRequest) onProviders(inventory);
    } catch (error) {
      if (!stopped && id === providerRequest) {
        onError(error.message);
        // The last invalidation may be the final one; retry even without another session event.
        providerRetry = setTimeout(refreshProviders, 2000);
      }
    }
  }
  async function refresh() {
    const id = ++requestId;
    void refreshProviders();
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
  return { refresh, close() { stopped = true; clearTimeout(providerRetry); events.close(); } };
}

export function formValues(schema, form) {
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
