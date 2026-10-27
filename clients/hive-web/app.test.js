import { afterEach, beforeEach, test } from 'node:test';
import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { catalog, connect, conversation, validateContent } from './src/client.js';

const session = { sessionId: 'hive-tui', cwd: '/workspace', agent: 'pi', title: 'Review parser',
  activity: 'idle', controlOwner: 'tui-connection', lastActivityAt: 1791754566280 };

const modelOptions = currentValue => [{ configId: 'model', name: 'Model', category: 'model', type: 'select', currentValue,
  options: [{ groupId: 'provider', name: 'Provider', options: [{ value: 'fast', name: 'Fast' }, { value: 'careful', name: 'Careful' }] }] }];

test('older provider responses cannot overwrite a newer catalog refresh', async () => {
  const pending = [];
  let revision = 1;
  globalThis.fetch = async url => url.includes('providers')
    ? new Promise(resolve => pending.push(inventory => resolve({ ok: true, json: async () => inventory })))
    : { ok: true, json: async () => ({ revision, sessions: [] }) };
  let source;
  globalThis.EventSource = class {
    constructor() { source = this; }
    addEventListener(_, listener) { this.revision = listener; }
    close() {}
  };
  const applied = [];
  const feed = catalog(() => {}, () => {}, inventory => applied.push(inventory));
  await new Promise(resolve => setImmediate(resolve));
  source.revision({ data: String(++revision) });
  pending[1]({ agents: [{ id: 'latest' }] });
  await new Promise(resolve => setImmediate(resolve));
  pending[0]({ agents: [{ id: 'obsolete' }] });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(applied, [{ agents: [{ id: 'latest' }] }]);
  feed.close();
});

test('failed initialization rejects pending clicks and reconnect opens only the latest desired session', async () => {
  const sockets = transport((socket, message) => {
    if (socket === sockets[0] && message.method === 'initialize') return false;
  });
  const client = connect('ws://localhost/acp');
  const first = client.open(session).catch(error => error);
  const latest = { ...session, sessionId: 'hive-latest' };
  const second = client.open(latest).catch(error => error);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().pendingSession, latest.sessionId);
  assert.match(client.state().status, /Opening/);
  sockets[0].close();
  assert.match((await first).message, /connection failed/);
  await second;
  await new Promise(resolve => setTimeout(resolve, 800));
  assert.equal(client.state().session.sessionId, latest.sessionId);
  assert.equal(client.state().role, 'observer');
  assert.deepEqual(sockets[1].sent.map(message => message.method), ['initialize', 'session/resume']);
  client.close();
});

test('an explicit session can select its model before the first inference and blocks prompts during the change', async () => {
  let setter;
  const sockets = transport((socket, message, result) => {
    if (message.method === 'session/new') return { ...result, configOptions: modelOptions('fast') };
    if (message.method === 'session/set_config_option') { setter = message; return false; }
  });
  const client = connect('ws://localhost/acp');
  await client.create('opencode');
  assert.equal(client.state().configOptions[0].currentValue, 'fast');
  assert.equal(sockets[0].sent.some(message => message.method === 'session/prompt'), false);
  const changing = client.configure('model', 'careful');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().configuring, true);
  await assert.rejects(client.prompt('go'), /configuration to finish/);
  assert.deepEqual(setter.params, { sessionId: 'hive-new', configId: 'model', type: 'id', value: 'careful', _meta: { hive: { generation: 1, token: 'new-token' } } });
  sockets[0].receive({ jsonrpc: '2.0', id: setter.id, result: { configOptions: modelOptions('careful'), _meta: { hive: { configSeq: 3 } } } });
  await changing;
  assert.equal(client.state().configOptions[0].currentValue, 'careful');
  await client.prompt('go');
  assert.deepEqual(sockets[0].sent.map(message => message.method), ['initialize', 'session/new', 'session/set_config_option', 'session/prompt']);
  client.close();
});

test('configuration snapshots cannot be regressed by older replay and failures render once', () => {
  const state = conversation();
  state.reconcileConfig(modelOptions('careful'), 5);
  state.apply(update(1, { sessionUpdate: 'config_option_update', configOptions: modelOptions('fast') }).params);
  assert.equal(state.configOptions[0].currentValue, 'careful');
  const failure = update(2, { sessionUpdate: '_hive_error', message: 'API key expired', help: 'Replace the key' }).params;
  assert.equal(state.apply(failure), true);
  assert.equal(state.apply(failure), false);
  state.apply(update(3, { sessionUpdate: 'state_update', state: 'idle', stopReason: '_v1_error' }).params);
  assert.deepEqual(state.lines, [{ kind: 'notice', text: 'API key expired — Replace the key' }]);
});

test('browser and native form validation obey the same conformance cases', () => {
  const cases = JSON.parse(readFileSync(new URL('../../crates/hive-protocol/tests/fixtures/elicitation.json', import.meta.url)));
  for (const item of cases) {
    if (item.valid) assert.doesNotThrow(() => validateContent(item.schema, item.content), item.name);
    else assert.throws(() => validateContent(item.schema, item.content), undefined, item.name);
  }
});

test('native failure companions remain visible once and restoring failure history preserves active work', () => {
  const state = conversation();
  const native = { sessionUpdate: 'state_update', state: 'idle', stopReason: 'error',
    error: { code: -32603, message: 'sk-must-not-leak', data: { token: 'sk-must-not-leak' } } };
  const diagnostic = { sessionUpdate: '_hive_error', version: 1, source: 'acp-v2', operation: 'foreground',
    code: 'hive::agent::foreground_failed', message: 'The downstream agent reported a foreground failure',
    help: "Check the agent's provider credentials and model configuration, and its logs; then send a new prompt" };
  state.apply(update(1, { sessionUpdate: 'state_update', state: 'requires_action' }).params);
  assert.equal(state.runningNow, true);
  state.apply(update(2, native).params);
  assert.equal(state.runningNow, false);
  assert.equal(state.apply(update(3, diagnostic).params), true);
  assert.equal(state.apply(update(3, diagnostic).params), false);
  state.apply(update(4, native).params);
  assert.equal(state.lines.length, 1);
  assert.match(state.lines[0].text, /foreground failure/);
  assert.doesNotMatch(JSON.stringify(state.lines), /sk-must-not-leak/);
  state.running(true);
  state.restore([update(2048, diagnostic).params, update(2048, diagnostic).params], 2048);
  // Distinct historical failures may share the boundary and have identical diagnostics.
  assert.equal(state.lines.length, 2);
  assert.equal(state.runningNow, true);
});

test('idle always ends foreground work without inventing an outcome from unreported reasons', () => {
  for (const stopReason of [undefined, null, {}, '_future', 'future']) {
    const state = conversation();
    state.running(true);
    state.apply(update(1, { sessionUpdate: 'state_update', state: 'idle', stopReason }).params);
    assert.equal(state.runningNow, false);
    assert.deepEqual(state.lines, []);
  }
});

test('native snapshot and normalized foreground keep blocked work busy and fence stale replay', () => {
  const state = conversation();
  for (const [index, foreground] of ['requires_action', 'running', 'requires_action', 'idle'].entries()) {
    state.apply(update(index + 1, { sessionUpdate: 'state_update', state: foreground }).params);
    assert.equal(state.runningNow, foreground !== 'idle');
    assert.equal(state.activity.foreground, foreground);
  }
  state.reconcile({ version: 1, seq: 10, foreground: 'requires_action' });
  assert.equal(state.runningNow, true);
  state.apply(update(5, { sessionUpdate: 'state_update', state: 'idle' }).params);
  assert.equal(state.runningNow, true, 'old replay cannot undo attachment evidence');
  state.skip(11);
  for (const [index, foreground] of ['idle', 'requires_action', '_future'].entries()) {
    state.apply(update(index + 11, { sessionUpdate: '_hive_activity', kind: 'foreground', state: { state: foreground } }).params);
    assert.equal(state.runningNow, foreground === 'requires_action');
    assert.equal(state.activity.foreground, foreground === '_future' ? 'unknown' : foreground);
  }
});

test('an inbound request cannot resolve an outbound request with the same id', async () => {
  const sockets = transport((socket, message) => {
    if (!message.method || message.method === 'session/prompt') return false;
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  let acknowledged = false;
  const prompt = client.prompt('go').then(() => { acknowledged = true; });
  await new Promise(resolve => setImmediate(resolve));
  const sent = sockets[0].sent.find(message => message.method === 'session/prompt');
  sockets[0].receive({ jsonrpc: '2.0', id: sent.id, method: 'session/request_permission', params: { sessionId: session.sessionId, title: 'Run?', options: [{ optionId: 'allow', name: 'Allow once' }] } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(acknowledged, false);
  assert.equal(client.respond({ outcome: { outcome: 'selected', optionId: 'invented' } }, sent.id), false);
  assert.equal(client.respond({ outcome: { outcome: 'cancelled' } }, sent.id), true);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(acknowledged, false);
  sockets[0].receive({ jsonrpc: '2.0', id: sent.id, result: { messageId: 'ack' } });
  await prompt;
  client.close();
});

test('queued interactions retain their ids and cancellation withdraws only its target', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  for (const id of ['first', 'second', 'third']) sockets[0].receive({ jsonrpc: '2.0', id, method: 'session/request_permission', params: { sessionId: session.sessionId, title: id, options: [] } });
  assert.equal(client.state().interaction.id, 'first');
  sockets[0].receive({ jsonrpc: '2.0', method: '$/cancel_request', params: { requestId: 'second' } });
  assert.equal(client.respond({ outcome: { outcome: 'cancelled' } }, 'first'), true);
  assert.equal(client.state().interaction.id, 'third');
  sockets[0].receive({ jsonrpc: '2.0', method: '$/cancel_request', params: { requestId: 'third' } });
  assert.equal(client.state().interaction, undefined);
  assert.equal(client.respond({ outcome: { outcome: 'selected', optionId: 'allow' } }, 'third'), false);
  client.close();
});

test('URL completion can arrive before create or acceptance and remains visible afterwards', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  sockets[0].receive({ jsonrpc: '2.0', method: 'elicitation/complete', params: { elicitationId: 'flow' } });
  sockets[0].receive({ jsonrpc: '2.0', id: 'url', method: 'elicitation/create', params: { sessionId: session.sessionId, mode: 'url', elicitationId: 'flow', url: 'https://example.com', message: 'Authenticate' } });
  sockets[0].receive({ jsonrpc: '2.0', method: 'elicitation/complete', params: { elicitationId: 'flow' } });
  assert.equal(client.respond({ action: 'accept' }, 'url'), true);
  assert.equal(client.state().urlFlows[0].complete, true);
  sockets[0].receive({ jsonrpc: '2.0', method: 'elicitation/complete', params: { elicitationId: 'unknown' } });
  assert.equal(client.state().urlFlows.length, 1);
  client.close();
});

test('a live request scoped form can be answered before session creation completes', async () => {
  let create;
  const sockets = transport((socket, message, result) => {
    if (message.method === 'session/new') { create = { id: message.id, result }; return false; }
  });
  const client = connect('ws://localhost/acp');
  const prompting = client.prompt('go');
  await new Promise(resolve => setImmediate(resolve));
  sockets[0].receive({ jsonrpc: '2.0', id: 'form', method: 'elicitation/create', params: { mode: 'form', requestId: create.id, message: 'Configure', requestedSchema: { properties: { enabled: { type: 'boolean' } }, required: ['enabled'] } } });
  assert.equal(client.state().interaction.requestScoped, true);
  assert.equal(client.respond({ action: 'accept', content: { enabled: false } }, 'form'), true);
  sockets[0].receive({ jsonrpc: '2.0', id: create.id, result: create.result });
  await prompting;
  client.close();
});

function update(seq, payload, id = session.sessionId) {
  return { jsonrpc: '2.0', method: 'session/update', params: { sessionId: id, update: payload,
    _meta: { hive: { seq } } } };
}

function transport(respond = () => {}) {
  const sockets = [];
  class Socket {
    static OPEN = 1;
    readyState = 0;
    sent = [];
    constructor() { sockets.push(this); queueMicrotask(() => { this.readyState = 1; this.onopen(); }); }
    send(raw) {
      const message = JSON.parse(raw);
      this.sent.push(message);
      if (message.id == null) return;
      let result;
      if (message.method === 'initialize') result = { protocolVersion: 2, capabilities: { session: {} } };
      else if (message.method === 'session/new') result = { sessionId: 'hive-new', _meta: { hive: { generation: 1,
        token: 'new-token', role: 'pilot', nextSeq: 1, running: false } } };
      else if (message.method === 'session/resume') result = { _meta: { hive: {
        generation: 3, ...(message.params._meta?.hive?.mode === 'observe' ? {} : { token: 'pilot-token' }),
        role: message.params._meta?.hive?.mode === 'observe' ? 'observer' : 'pilot',
        nextSeq: 11, running: false } } };
      else result = { messageId: 'user-1' };
      const action = respond(this, message, result);
      if (action === false) return;
      queueMicrotask(() => this.receive({ jsonrpc: '2.0', id: message.id, result: action || result }));
    }
    receive(message) { this.onmessage({ data: JSON.stringify(message) }); }
    close() { this.readyState = 3; this.onclose(); }
  }
  globalThis.WebSocket = Socket;
  return sockets;
}

beforeEach(() => {
  const values = new Map();
  globalThis.localStorage = {
    getItem: key => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: key => values.delete(key),
  };
});

afterEach(() => {
  delete globalThis.WebSocket;
  delete globalThis.document;
  delete globalThis.location;
  delete globalThis.history;
  delete globalThis.FormData;
  delete globalThis.EventSource;
  delete globalThis.fetch;
  delete globalThis.localStorage;
});

test('opening a TUI session observes its original cwd until the user types', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.open(session);
  assert.deepEqual(sockets[0].sent.map(item => item.method), ['initialize', 'session/resume']);
  assert.equal(sockets[0].sent[1].params.cwd, '/workspace');
  assert.equal(sockets[0].sent[1].params._meta.hive.mode, 'observe');
  assert.equal(client.state().role, 'observer');
  assert.equal(client.cancel(), false);
  await assert.rejects(client.prompt('new question'), /take control/);
  await client.takeControl();
  await client.prompt('new question');
  const prompt = sockets[0].sent.at(-1);
  assert.equal(prompt.method, 'session/prompt');
  assert.equal(prompt.params.sessionId, 'hive-tui');
  assert.deepEqual(prompt.params._meta.hive.generation, 3);
  assert.equal(prompt.params._meta.hive.token, 'pilot-token');
  assert.ok(prompt.params._meta.hive.key);
  client.close();
});

test('an interrupted session only acknowledges recovery after an explicit user action', async () => {
  const sockets = transport((_, message, result) => message.method === 'session/resume'
    ? { _meta: { hive: { ...result._meta.hive, recoveryRequired: !message.params._meta?.hive?.recoveryAction } } }
    : undefined);
  const client = connect('ws://localhost/acp');
  await client.open({ ...session, recovery: 'recovery_required' });
  assert.equal(client.state().recoveryRequired, true);
  assert.equal(sockets[0].sent.filter(message => message.params?._meta?.hive?.recoveryAction).length, 0);
  await client.acknowledgeRecovery();
  assert.equal(sockets[0].sent.filter(message => message.params?._meta?.hive?.recoveryAction).length, 1);
  assert.equal(client.state().role, 'pilot');
  assert.equal(client.state().recoveryRequired, false);
  client.close();
});

test('a new browser session is created only on its first prompt', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.prompt('hello', 'pi');
  assert.deepEqual(sockets[0].sent.map(item => item.method), ['initialize', 'session/new', 'session/prompt']);
  assert.equal(sockets[0].sent[1].params._meta.hive.agent, 'pi');
  assert.equal(client.state().session.sessionId, 'hive-new');
  client.close();
});

test('a lost session creation acknowledgement cannot silently create a second agent session', async () => {
  const sockets = transport((_, message) => message.method === 'session/new' ? false : undefined);
  const client = connect('ws://localhost/acp');
  const pending = client.prompt('hello');
  await new Promise(resolve => setImmediate(resolve));
  sockets[0].close();
  await assert.rejects(pending, /disconnected/);
  await new Promise(resolve => setTimeout(resolve, 800));
  await assert.rejects(client.prompt('hello'), /creation was interrupted/);
  assert.equal(sockets[1].sent.filter(item => item.method === 'session/new').length, 0);
  client.close();
});

test('replay folds chunks into full messages without duplicating them', () => {
  const history = conversation();
  assert.equal(history.apply(update(1, { sessionUpdate: 'agent_message_chunk', messageId: 'a', content: { type: 'text', text: 'Hello ' } }).params), true);
  assert.equal(history.apply(update(1, { sessionUpdate: 'agent_message_chunk', messageId: 'a', content: { type: 'text', text: 'Hello ' } }).params), false);
  history.apply(update(2, { sessionUpdate: 'agent_message_chunk', messageId: 'a', content: { type: 'text', text: 'world' } }).params);
  history.apply(update(3, { sessionUpdate: 'agent_message', messageId: 'a', content: [{ type: 'text', text: 'Hello world' }] }).params);
  history.apply(update(4, { sessionUpdate: 'user_message', messageId: 'u', content: [{ type: 'text', text: 'hi' }] }).params);
  history.apply(update(5, { sessionUpdate: 'state_update', state: 'running' }).params);
  assert.equal(history.runningNow, true);
  history.apply(update(6, { sessionUpdate: 'state_update', state: 'idle' }).params);
  assert.deepEqual(history.lines, [{ kind: 'agent', text: 'Hello world' }, { kind: 'user', text: 'hi' }]);
  assert.equal(history.runningNow, false);
});

test('only live thinking blocks remain active until conversational output or idle completes them', () => {
  const history = conversation();
  let seq = 0;
  const apply = value => history.apply(update(++seq, value).params);
  const thought = messageId => ({ sessionUpdate: 'agent_thought_chunk', messageId, content: { type: 'text', text: 'Reasoning' } });
  apply(thought('first'));
  apply({ sessionUpdate: '_hive_usage', session: { turns: 1 } });
  assert.equal(history.lines[0].active, true);
  apply(thought('second'));
  assert.equal(history.lines[0].active, false);
  assert.equal(history.lines[1].active, true);
  apply({ sessionUpdate: 'agent_thought', messageId: 'second', content: [] });
  assert.equal(history.lines[1].active, false);
  apply(thought('third'));
  apply({ sessionUpdate: 'tool_call_update', toolCallId: 'read', status: 'in_progress' });
  assert.equal(history.lines[2].active, false);
  apply(thought('fourth'));
  apply({ sessionUpdate: 'state_update', state: 'idle' });
  apply({ sessionUpdate: 'state_update', state: 'running' });
  assert.equal(history.lines.filter(line => line.active).length, 0);
  history.restore([update(seq, { sessionUpdate: 'agent_thought', messageId: 'restored', content: [{ type: 'text', text: 'Saved thought' }] }).params], seq);
  assert.equal(history.lines[0].active, false);
  apply(thought('restored'));
  assert.equal(history.lines[0].active, true);
  apply({ sessionUpdate: 'agent_message_chunk', messageId: 'answer', content: { type: 'text', text: 'Answer' } });
  assert.equal(history.lines[0].active, false);
});

test('opening stays busy through attachment and clears on failure including same session recovery', async () => {
  let held;
  const sockets = transport((socket, message, result) => {
    if (message.method === 'session/resume') { held = () => socket.receive({ id: message.id, result }); return false; }
  });
  const client = connect('ws://localhost/acp');
  const opening = client.open(session);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().session.sessionId, session.sessionId);
  assert.equal(client.state().opening, true);
  held(); await opening;
  assert.equal(client.state().opening, false);
  const taking = client.takeControl();
  assert.equal(client.state().opening, true);
  held(); await taking;
  assert.equal(client.state().opening, false);
  const failed = client.open({ ...session, sessionId: 'missing' });
  await new Promise(resolve => setImmediate(resolve));
  // Opening another session first relinquishes the current pilot.
  held();
  await new Promise(resolve => setImmediate(resolve));
  const request = sockets[0].sent.at(-1);
  sockets[0].receive({ id: request.id, error: { message: 'Attachment failed' } });
  await assert.rejects(failed, /Attachment failed/);
  assert.equal(client.state().opening, false);
  client.close();
});

test('stopping waits for authoritative idle and disconnect clears cancellation animation', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.open(session); await client.takeControl(); await client.prompt('work');
  assert.equal(client.cancel(), true);
  assert.equal(client.state().stopping, true);
  assert.equal(client.state().running, true);
  sockets[0].receive(update(1, { sessionUpdate: 'state_update', state: 'idle' }));
  assert.equal(client.state().stopping, false);
  assert.equal(client.state().running, false);
  await client.prompt('more'); client.cancel();
  sockets[0].close();
  assert.equal(client.state().stopping, false);
  client.close();
});

test('usage snapshots and repricing events replace totals without adding a replayed turn', () => {
  const view = conversation();
  view.reconcileUsage({ turns: 1, tokens: { totalTokens: 120 }, actualSessionCost: null }, 4);
  view.skip(5);
  assert.equal(view.apply(update(5, { sessionUpdate: '_hive_usage', turn: { id: 'one' },
    session: { turns: 1, tokens: { totalTokens: 120 }, estimatedCost: { amount: 0.004, currency: 'USD' } } }).params), true);
  assert.equal(view.apply(update(5, { sessionUpdate: '_hive_usage', session: { turns: 2 } }).params), false);
  view.reconcileUsage({ turns: 0 }, 3);
  assert.equal(view.usage.turns, 1);
  view.apply(update(6, { sessionUpdate: '_hive_usage', turn: null,
    session: { turns: 1, tokens: { totalTokens: 120 }, actualSessionCost: { amount: 0.003, currency: 'USD' } } }).params);
  assert.equal(view.usage.actualSessionCost.amount, 0.003);
});

test('a fresh page restores multi-turn snapshots before live events without issuing new prompts', async () => {
  const sockets = transport((socket, message, result) => {
    if (message.method !== 'session/resume') return;
    if (message.params.replayFrom?.type !== 'start') return;
    for (const [index, text] of ['first', 'answer', 'second', 'response'].entries()) {
      const event = update(2048, { sessionUpdate: index % 2 ? 'agent_message' : 'user_message',
        messageId: String(index), content: [{ type: 'text', text }] });
      event.params._meta.hive.snapshot = true;
      socket.receive(event);
    }
    socket.receive(update(2049, { sessionUpdate: 'agent_message_chunk', messageId: '3', content: { type: 'text', text: '!' } }));
    return { ...result, _meta: { hive: { ...result._meta.hive, snapshotThroughSeq: 2048, nextSeq: 2050 } } };
  });
  for (let reload = 0; reload < 2; reload++) {
    const client = connect('ws://localhost/acp');
    await client.open(session);
    assert.deepEqual(client.state().lines.map(line => line.text), ['first', 'answer', 'second', 'response!']);
    // Opening an already-owned view is a no-op; switch away to exercise a real new attachment.
    await client.open({ ...session, sessionId: 'hive-other' });
    await client.open(session);
    assert.deepEqual(client.state().lines.map(line => line.text), ['first', 'answer', 'second', 'response!']);
    assert.equal(client.state().gap, false);
    client.close();
  }
  assert.equal(sockets.flatMap(socket => socket.sent).some(message => ['session/new', 'session/prompt'].includes(message.method)), false);
  const attachments = sockets.flatMap(socket => socket.sent).filter(message => message.method === 'session/resume');
  assert.equal(attachments.filter(message => message.params.sessionId === session.sessionId).length, 4);
  assert.ok(attachments.every(message => message.params.cwd === session.cwd));
});

test('empty restored snapshots establish their cursor without hiding a takeover event', () => {
  const history = conversation();
  history.restore([], 4096);
  assert.equal(history.seq, 4096);
  history.apply(update(4097, { sessionUpdate: '_hive_control', generation: 8, pilot: true }).params);
  assert.equal(history.generation, 8);
  assert.equal(history.gap, false);
});

test('transcript coverage survives older replay and live updates without blocking partial history', async () => {
  const partial = { owner: 'agent', source: 'agent_history', restoration: 'supported', availability: 'available',
    completeness: 'partial', warnings: [{ code: 'hive::transcript::truncated', message: 'Earlier content is retained by the agent' }] };
  transport((socket, message, result) => {
    if (message.method !== 'session/resume') return;
    const event = update(20, { sessionUpdate: 'agent_message', messageId: 'retained', content: [{ type: 'text', text: 'Retained answer' }] });
    event.params._meta.hive.snapshot = true;
    socket.receive(event);
    return { ...result, _meta: { hive: { ...result._meta.hive, transcript: partial, transcriptSeq: 20, snapshotThroughSeq: 20, nextSeq: 21 } } };
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  assert.equal(client.state().gap, false);
  assert.equal(client.state().lines[0].text, 'Retained answer');
  assert.deepEqual(client.state().transcript, partial);
  client.close();
  const view = conversation();
  view.reconcileTranscript(partial, 20);
  view.apply(update(1, { sessionUpdate: '_hive_control', generation: 1, pilot: false,
    transcript: { ...partial, completeness: 'unknown', warnings: [] } }).params);
  view.apply(update(2, { sessionUpdate: 'agent_message_chunk', messageId: 'live', content: { type: 'text', text: 'New content' } }).params);
  assert.deepEqual(view.transcript, partial);
});

for (const foreground of ['running', 'idle', 'requires_action']) {
  test(`resume barriers and response snapshots agree for ${foreground} with and without replay`, async () => {
    for (const replay of [true, false]) {
      transport((socket, message, result) => {
        if (message.method !== 'session/resume') return;
        if (!replay && message.params.replayFrom) {
          queueMicrotask(() => socket.receive({ jsonrpc: '2.0', id: message.id,
            error: { code: -32600, message: 'replay cursor unavailable' } }));
          return false;
        }
        socket.receive(update(11, { sessionUpdate: 'state_update', state: foreground }));
        return { ...result, _meta: { hive: { ...result._meta.hive, nextSeq: 12,
          running: foreground !== 'idle',
          ...(message.params.replayFrom ? { snapshotThroughSeq: 10 } : {}),
          activitySnapshot: { version: 1, seq: 11, foreground, session: 'active', tools: {}, plans: {}, tasks: {} } } } };
      });
      const client = connect('ws://localhost/acp');
      if (replay) await client.open(session);
      else {
        await assert.rejects(client.open(session), /replay cursor unavailable/);
        await client.attachWithoutHistory();
      }
      assert.equal(client.state().running, foreground !== 'idle');
      assert.equal(client.state().activity.foreground, foreground);
      assert.equal(client.state().gap, false);
      client.close();
    }
  });
}

test('failed snapshot attachments discard staged messages and failures before explicit live-only attachment', async () => {
  let rejected = false;
  const sockets = transport((socket, message) => {
    if (message.method !== 'session/resume' || !message.params.replayFrom) return;
    const event = update(2048, { sessionUpdate: 'agent_message', messageId: 'uncommitted', content: [{ type: 'text', text: 'discard' }] });
    event.params._meta.hive.snapshot = true;
    socket.receive(event);
    const failure = update(2048, { sessionUpdate: '_hive_error', source: 'acp-v2',
      message: 'Uncommitted failure', help: 'Check the agent logs' });
    failure.params._meta.hive.snapshot = true;
    socket.receive(failure);
    socket.receive(update(2049, { sessionUpdate: '_hive_control', generation: 1, pilot: false,
      transcript: { completeness: 'partial', warnings: [{ message: 'Uncommitted status' }] } }));
    queueMicrotask(() => {
      socket.receive({ jsonrpc: '2.0', id: message.id, error: { code: -32600, message: 'Unsupported history', data: 'hive::transcript::unsupported: attach without earlier history' } });
      rejected = true;
    });
    return false;
  });
  const client = connect('ws://localhost/acp');
  await assert.rejects(client.open(session), /hive::transcript::unsupported/);
  assert.equal(rejected, true);
  assert.equal(client.state().gap, true);
  assert.deepEqual(client.state().lines, []);
  assert.equal(client.state().transcript, undefined);
  await client.attachWithoutHistory();
  assert.equal(client.state().gap, false);
  assert.equal(sockets[0].sent.at(-1).params.replayFrom, undefined);
  assert.equal(client.state().lines.some(line => line.text === 'discard'), false);
  assert.equal(client.state().lines.some(line => line.text.includes('Uncommitted failure')), false);
  client.close();
});

for (const [kind, speaker] of [['user_message', 'user'], ['agent_message', 'agent'], ['agent_thought', 'thought']]) {
  test(`${kind} patches preserve omissions and clear null before replacements and later chunks`, () => {
    const view = conversation();
    let seq = 0;
    const apply = fields => view.apply(update(++seq, { sessionUpdate: kind, messageId: 'same', ...fields }).params);
    apply({});
    view.apply(update(++seq, { sessionUpdate: 'user_message', messageId: 'other', content: [{ type: 'text', text: 'next' }] }).params);
    const chunk = text => ({ sessionUpdate: `${kind}_chunk`, messageId: 'same', content: { type: 'text', text } });
    view.apply(update(++seq, chunk('draft')).params);
    apply({ _meta: { reviewed: true } });
    assert.equal(view.lines[0].text, 'draft');
    apply({ content: null });
    assert.equal(view.lines[0].text, '');
    apply({});
    assert.equal(view.lines[0].text, '');
    apply({ content: [{ type: 'text', text: 'final' }] });
    view.apply(update(++seq, chunk('!')).params);
    assert.equal(view.lines[0].text, 'final!');
    assert.equal(view.lines[0].kind, speaker);
    assert.equal(view.lines[1].text, 'next');
    const restored = conversation();
    restored.restore([
      update(seq, { sessionUpdate: kind, messageId: 'same', content: [{ type: 'text', text: 'final!' }] }).params,
      update(seq, { sessionUpdate: 'user_message', messageId: 'other', content: [{ type: 'text', text: 'next' }] }).params,
    ], seq);
    // Whole-message upserts end live thinking before comparison with inactive snapshots.
    apply({});
    restored.apply(update(seq, { sessionUpdate: kind, messageId: 'same' }).params);
    assert.deepEqual(restored.lines, view.lines);
    const later = update(++seq, chunk('?')).params;
    for (const client of [view, restored]) {
      assert.equal(client.apply(later), true);
      assert.equal(client.apply(later), false);
    }
    assert.deepEqual(restored.lines, view.lines);
    apply({ content: [] });
    assert.equal(view.lines[0].text, '');
  });
}

test('message speaker namespaces keep identical ids independent', () => {
  const view = conversation();
  ['user_message', 'agent_message', 'agent_thought'].forEach((kind, index) => {
    view.apply(update(index + 1, { sessionUpdate: kind, messageId: 'same', content: [{ type: 'text', text: kind }] }).params);
  });
  assert.deepEqual(view.lines.map(line => line.text), ['user_message', 'agent_message', 'agent_thought']);
});

for (const wrapped of [false, true]) {
  test(`${wrapped ? 'translated v1' : 'native v2'} tools clear fields without resurrecting output on patches or restore`, () => {
    const view = conversation();
    let seq = 0;
    const state = { toolCallId: 'call' };
    const notification = tool => wrapped ? { sessionUpdate: '_hive_v1_tool_update', tool } : { sessionUpdate: 'tool_call_update', ...tool };
    const patch = fields => {
      Object.assign(state, fields);
      view.apply(update(++seq, notification({ toolCallId: 'call', ...fields })).params);
      const restored = conversation();
      restored.restore([update(seq, notification(state)).params], seq);
      assert.deepEqual(restored.lines, view.lines);
      return restored;
    };
    patch({ title: 'Read', status: 'in_progress', rawInput: { path: 'file' }, rawOutput: 'fallback' });
    const content = [{ type: 'content', content: { type: 'text', text: 'structured' } }];
    patch({ content });
    patch({ status: 'completed' });
    assert.equal(view.lines[0].text, 'structured');
    patch({ title: null, status: null, rawInput: null, content: null });
    assert.deepEqual(view.lines, [{ kind: 'tool', text: 'fallback', title: 'Tool activity', status: 'unknown' }]);
    patch({ content: [] });
    assert.equal(view.lines[0].text, 'fallback');
    patch({ rawOutput: null });
    patch({ status: 'completed' });
    assert.equal(view.lines[0].text, '');
    patch({ content });
    assert.equal(view.lines[0].text, 'structured');
    const restored = patch({ content: null });
    if (!wrapped) {
      const chunk = update(++seq, { sessionUpdate: 'tool_call_content_chunk', toolCallId: 'call', content: content[0] }).params;
      view.apply(chunk);
      restored.apply(chunk);
      assert.deepEqual(restored.lines, view.lines);
      assert.equal(view.lines[0].text, 'structured');
    }
    view.restore([update(seq, notification({ toolCallId: 'call' })).params], seq);
    view.apply(update(++seq, notification({ toolCallId: 'call', status: 'completed' })).params);
    assert.equal(view.lines[0].text, '');
    assert.equal(view.lines[0].title, 'Tool activity');
  });
}

test('tool status patches preserve readable output and the original title', () => {
  const view = conversation();
  view.apply(update(1, { sessionUpdate: 'tool_call_update', toolCallId: 'call', title: 'Check files', status: 'in_progress', rawInput: { command: 'ls' } }).params);
  view.apply(update(2, { sessionUpdate: 'tool_call_update', toolCallId: 'call', content: [{ type: 'content', content: { type: 'text', text: 'first\nsecond' } }] }).params);
  view.apply(update(3, { sessionUpdate: 'tool_call_update', toolCallId: 'call', status: 'completed' }).params);
  assert.deepEqual(view.lines, [{ kind: 'tool', title: 'Check files', status: 'completed', input: '{\n  "command": "ls"\n}', text: 'first\nsecond' }]);
});

test('native plans preserve replacements metadata and ordering independently of activity', () => {
  const view = conversation();
  const plan = entries => ({ sessionUpdate: 'plan_update', _meta: { provider: true },
    plan: { type: 'items', planId: 'steps', _meta: { plan: true }, entries } });
  const first = plan([{ content: 'A', status: 'pending', priority: 'high', _meta: { id: 'a' } },
    { content: 'B', status: 'in_progress', priority: 'future' }]);
  view.apply(update(1, first).params);
  assert.deepEqual(view.planSnapshot.plans.steps, first);
  const second = plan([first.plan.entries[1], { ...first.plan.entries[0], status: 'cancelled' }]);
  view.apply(update(2, second).params);
  assert.deepEqual(view.planSnapshot.plans.steps, second);
  view.apply(update(3, plan([])).params);
  assert.deepEqual(view.planSnapshot.plans.steps.plan.entries, []);
  const unknown = { sessionUpdate: 'plan_update', plan: { type: '_future', planId: 'other', payload: [2, 1] } };
  view.apply(update(4, unknown).params);
  assert.deepEqual(view.planSnapshot.plans.other, unknown);
  assert.equal(view.planSnapshot.observation, 'observed');
  assert.equal(view.planSnapshot.support, 'supported');
  assert.equal(view.planSnapshot.authoritative, false);
  assert.equal(view.activity.support.plans, 'unknown');
  view.reconcilePlans({ version: 1, seq: 1, plans: {} });
  assert.deepEqual(view.planSnapshot.plans.other, unknown);
});

test('plan snapshots fence older replay while preserving newer pre-acknowledgement updates', async () => {
  const old = { sessionUpdate: 'plan_update', plan: { type: 'items', planId: 'steps', entries: [] } };
  const current = { ...old, _meta: { preserved: true }, plan: { ...old.plan, entries: [{ content: 'Current', status: 'completed', priority: 'high' }] } };
  const view = conversation();
  view.reconcilePlans({ version: 1, seq: 10, support: 'supported', observation: 'observed', plans: { steps: current } });
  assert.equal(view.planSnapshot.authoritative, true);
  view.apply(update(1, old).params);
  assert.deepEqual(view.planSnapshot.plans.steps, current);
  assert.equal(view.planSnapshot.seq, 10);
  const sockets = transport((socket, message, result) => {
    if (message.method === 'session/resume') {
      result._meta.hive.planSnapshot = { version: 1, seq: 10, support: 'supported', observation: 'observed', plans: { steps: current } };
      queueMicrotask(() => {
        socket.receive(update(1, old));
        // Contiguous unrelated events must still pass even though the plan watermark is ahead.
        for (let seq = 2; seq <= 10; seq++) socket.receive(update(seq, { sessionUpdate: 'state_update', state: 'idle' }));
        socket.receive(update(11, { ...old, plan: { ...old.plan, type: '_future', value: 'newer' } }));
      });
    }
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  assert.equal(client.state().planSnapshot.plans.steps.plan.value, 'newer');
  assert.equal(client.state().planSnapshot.seq, 11);
  assert.equal(client.state().gap, false);
  client.close();
  assert.equal(sockets.length, 1);
});

test('activity snapshots reconcile missed tools and later task updates without reopening idle', () => {
  const view = conversation();
  view.reconcile({ version: 1, seq: 14, foreground: 'idle', tools: {
    call: { toolCallId: 'call', title: 'Compile', status: 'completed' } },
  plans: { list: { entries: [{ content: 'Compile', status: 'completed' }] } }, tasks: {},
  support: { tools: 'observed', plans: 'observed', tasks: 'unknown' } });
  view.skip(15);
  assert.equal(view.apply(update(15, { sessionUpdate: '_hive_activity', kind: 'task', taskId: 'job',
    task: { taskId: 'job', status: 'completed' } }).params), true);
  assert.equal(view.apply(update(15, { sessionUpdate: '_hive_activity', kind: 'task', taskId: 'job' }).params), false);
  assert.equal(view.activity.foreground, 'idle');
  assert.equal(view.activity.tasks.job.status, 'completed');
  assert.equal(view.activity.tools.call.title, 'Compile');
  assert.equal(view.activity.support.tasks, 'observed');
  assert.equal(view.apply(update(17, { sessionUpdate: '_hive_activity', kind: 'tool' }).params), false);
  assert.equal(view.gap, true);
  assert.equal(view.seq, 15);
});

test('tool activity replacements agree with snapshots without reviving obsolete chunks on replay', () => {
  for (const content of [[{ type: 'content', content: { type: 'text', text: 'replacement' } }], [], null]) {
    const stale = { toolCallId: 'call', status: 'in_progress', chunks: [{ content: 'obsolete' }], chunksTruncated: true };
    const replacement = { toolCallId: 'call', status: 'in_progress', ...(content === null ? {} : { content }) };
    const later = { ...replacement, chunks: [{ content: 'later' }] };
    const activity = tool => ({ sessionUpdate: '_hive_activity', kind: 'tool', toolCallId: 'call', tool });
    const view = conversation();
    view.apply(update(1, activity(stale)).params);
    view.apply(update(2, activity(replacement)).params);
    assert.deepEqual(view.activity.tools.call, replacement);
    view.apply(update(3, activity(later)).params);
    assert.deepEqual(view.activity.tools.call, later);
    const attached = conversation();
    attached.reconcile(view.activity);
    attached.apply(update(1, activity(stale)).params);
    attached.apply(update(2, activity(replacement)).params);
    attached.apply(update(3, activity(later)).params);
    assert.deepEqual(attached.activity, view.activity);
    const newer = { ...later, chunks: [...later.chunks, { content: 'newest' }] };
    view.apply(update(4, activity(newer)).params);
    attached.apply(update(4, activity(newer)).params);
    assert.deepEqual(attached.activity, view.activity);
    assert.equal(view.lines.length, 0, 'normalized tool events must not duplicate transcript output');
  }
});

test('a resume snapshot is applied before activity that arrives before its acknowledgement', async () => {
  const sockets = transport((socket, message, result) => {
    if (message.method === 'session/resume') {
      result._meta.hive.nextSeq = 12;
      result._meta.hive.activitySnapshot = { version: 1, seq: 10, session: 'active',
        foreground: 'idle', tools: { call: { title: 'Compile', status: 'completed' } },
        plans: {}, tasks: {}, support: { tools: 'observed', plans: 'unknown', tasks: 'unknown' } };
      queueMicrotask(() => socket.receive(update(11, { sessionUpdate: '_hive_activity', kind: 'task',
        taskId: 'background', task: { status: 'completed' } })));
    }
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  assert.equal(client.state().activity.tools.call.status, 'completed');
  assert.equal(client.state().activity.tasks.background.status, 'completed');
  assert.equal(client.state().activity.foreground, 'idle');
  client.close();
  assert.equal(sockets.length, 1);
});

test('takeover replay precedes live events and fences an earlier pilot', async () => {
  const sockets = transport((socket, message) => {
    if (message.method === 'session/resume' && message.params.replayFrom?.type === 'start') {
      queueMicrotask(() => socket.receive(update(1, { sessionUpdate: 'agent_message_chunk', messageId: 'a', content: { text: 'before' } })));
    }
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  assert.deepEqual(client.state().lines, [{ kind: 'agent', text: 'before' }]);
  await client.takeControl();
  assert.deepEqual(sockets[0].sent[2].params.replayFrom, { type: '_hive_seq', seq: 2 });
  sockets[0].receive(update(2, { sessionUpdate: '_hive_control', generation: 4, pilot: true }));
  assert.equal(client.state().role, 'observer');
  assert.equal(client.cancel(), false);
  await assert.rejects(client.prompt('stale'), /take control/);
  client.close();
});

test('reconnect observes the same session and never resends an uncertain prompt', async () => {
  const sockets = transport((_, message) => message.method === 'session/prompt' ? false : undefined);
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  const prompt = client.prompt('question');
  await new Promise(resolve => setImmediate(resolve));
  const key = sockets[0].sent.at(-1).params._meta.hive.key;
  sockets[0].receive(update(10, { sessionUpdate: 'agent_message_chunk', messageId: 'a', content: { text: 'working' } }));
  sockets[0].close();
  await assert.rejects(prompt, /disconnected/);
  assert.equal(client.state().uncertain.key, key);
  await new Promise(resolve => setTimeout(resolve, 800));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(sockets.length, 2);
  assert.deepEqual(sockets[1].sent.map(item => item.method), ['initialize', 'session/resume']);
  assert.deepEqual(sockets[1].sent[1].params.replayFrom, { type: '_hive_seq', seq: 11 });
  assert.equal(sockets[1].sent[1].params.sessionId, 'hive-tui');
  assert.equal(client.state().role, 'observer');
  await client.takeControl();
  const retried = client.prompt('question');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(sockets[1].sent.at(-1).params._meta.hive.key, key);
  sockets[1].receive({ jsonrpc: '2.0', id: sockets[1].sent.at(-1).id, result: { messageId: 'user-1' } });
  await retried;
  assert.equal(client.state().uncertain, undefined);
  client.close();
});

test('expired replay leaves control unchanged until the user skips missing history', async () => {
  const sockets = transport((_, message) => message.method === 'session/resume' && message.params.replayFrom
    ? (queueMicrotask(() => sockets[0].receive({ jsonrpc: '2.0', id: message.id,
      error: { message: 'replay cursor unavailable; available next-event range is 4..=11' } })), false)
    : undefined);
  const client = connect('ws://localhost/acp');
  await assert.rejects(client.open(session), /replay cursor unavailable/);
  assert.equal(client.state().gap, true);
  assert.equal(client.cancel(), false);
  await client.attachWithoutHistory();
  assert.equal(sockets[0].sent.at(-1).params.replayFrom, undefined);
  assert.equal(client.state().seq, 10);
  assert.match(client.state().lines[0].text, /Earlier history unavailable/);
  client.close();
});

test('only the current pilot can answer permission and form or URL elicitation', async () => {
  const sockets = transport();
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  sockets[0].receive({ jsonrpc: '2.0', id: 17, method: 'session/request_permission',
    params: { sessionId: 'hive-tui', title: 'Run command', options: [{ name: 'Allow', optionId: 'allow' }] } });
  assert.equal(client.respond({ outcome: { outcome: 'selected', optionId: 'allow' } }, 17), true);
  assert.deepEqual(sockets[0].sent.at(-1).result, { outcome: { outcome: 'selected', optionId: 'allow' } });
  sockets[0].receive({ jsonrpc: '2.0', id: 18, method: 'elicitation/create',
    params: { sessionId: 'hive-tui', mode: 'form', message: 'Choose', requestedSchema: { properties: {} } } });
  sockets[0].receive(update(1, { sessionUpdate: '_hive_control', generation: 4, pilot: true }));
  assert.equal(client.state().interaction, undefined);
  assert.equal(client.respond({ action: 'accept', content: {} }, 18), false);
  await client.takeControl();
  sockets[0].receive({ jsonrpc: '2.0', id: 19, method: 'elicitation/create',
    params: { sessionId: 'hive-tui', mode: 'url', message: 'Visit URL', url: 'https://example.org' } });
  assert.equal(client.respond({ action: 'accept' }, 19), true);
  client.close();
});

test('agent input arriving just before takeover acknowledgement is still offered to the new pilot', async () => {
  const sockets = transport((socket, message) => {
    if (message.method === 'session/resume' && !message.params._meta?.hive?.mode) {
      queueMicrotask(() => socket.receive({ jsonrpc: '2.0', id: 40, method: 'session/request_permission',
        params: { sessionId: 'hive-tui', title: 'Approve', options: [] } }));
    }
  });
  const client = connect('ws://localhost/acp');
  await client.open(session);
  await client.takeControl();
  assert.equal(client.state().interaction.id, 40);
  assert.equal(client.respond({ outcome: { outcome: 'cancelled' } }, 40), true);
  assert.equal(sockets[0].sent.at(-1).id, 40);
  client.close();
});

test('rapid selections ignore obsolete attachment responses without forking a session', async () => {
  const sockets = transport((_, message) => message.method === 'session/resume' &&
    message.params.sessionId === 'hive-tui' ? false : undefined);
  const client = connect('ws://localhost/acp');
  const first = client.open(session);
  await new Promise(resolve => setImmediate(resolve));
  const second = client.open({ ...session, sessionId: 'hive-second' });
  const old = sockets[0].sent.find(message => message.method === 'session/resume');
  sockets[0].receive({ jsonrpc: '2.0', id: old.id, result: { _meta: { hive: {
    generation: 3, role: 'observer', running: false, nextSeq: 1,
  } } } });
  await Promise.all([first, second]);
  assert.equal(client.state().session.sessionId, 'hive-second');
  assert.equal(sockets[0].sent.filter(message => message.method === 'session/new').length, 0);
  assert.equal(sockets[0].sent.filter(message => message.method === 'session/resume').length, 2);
  client.close();
});

test('a takeover acknowledged after switching releases the abandoned pilot', async () => {
  const sockets = transport((_, message) => message.method === 'session/resume' &&
    message.params.sessionId === 'hive-tui' && !message.params._meta?.hive?.mode ? false : undefined);
  const client = connect('ws://localhost/acp');
  await client.open(session);
  const takeover = client.takeControl();
  await new Promise(resolve => setImmediate(resolve));
  const switchToSecond = client.open({ ...session, sessionId: 'hive-second' });
  await switchToSecond;
  const pending = sockets[0].sent.find(message => message.method === 'session/resume' &&
    message.params.sessionId === 'hive-tui' && !message.params._meta?.hive?.mode);
  sockets[0].receive({ jsonrpc: '2.0', id: pending.id, result: { _meta: { hive: {
    generation: 3, role: 'pilot', token: 'late-token', nextSeq: 1, running: false,
  } } } });
  await takeover;
  assert.equal(sockets[0].sent.at(-1).params._meta.hive.mode, 'relinquish');
  assert.equal(sockets[0].sent.at(-1).params._meta.hive.token, 'late-token');
  assert.equal(client.state().session.sessionId, 'hive-second');
  client.close();
});
