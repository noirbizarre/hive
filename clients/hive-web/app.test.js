import { afterEach, beforeEach, test } from 'node:test';
import { strict as assert } from 'node:assert';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { catalog, connect, conversation, mount, validateContent } from './app.js';

const session = { sessionId: 'hive-tui', cwd: '/workspace', agent: 'pi', title: 'Review parser',
  activity: 'idle', controlOwner: 'tui-connection', lastActivityAt: 1791754566280 };

const modelOptions = currentValue => [{ configId: 'model', name: 'Model', category: 'model', type: 'select', currentValue,
  options: [{ groupId: 'provider', name: 'Provider', options: [{ value: 'fast', name: 'Fast' }, { value: 'careful', name: 'Careful' }] }] }];

test('provider invalidations preserve selection and drafts without changing the attached conversation', async () => {
  const dom = new JSDOM(readFileSync(new URL('./index.html', import.meta.url), 'utf8'), { url: 'http://localhost/ui?session=hive-tui' });
  globalThis.document = dom.window.document;
  globalThis.location = dom.window.location;
  globalThis.history = dom.window.history;
  globalThis.FormData = dom.window.FormData;
  let revision = 1;
  let agents = [{ id: 'pi', name: 'Pi' }, { id: 'other', name: 'Other' }];
  globalThis.fetch = async url => ({ ok: true, json: async () => url.includes('providers')
    ? { agents, defaultAgent: agents[0]?.id } : { revision, sessions: [session] } });
  let source;
  globalThis.EventSource = class {
    constructor() { source = this; }
    addEventListener(_, listener) { this.revision = listener; }
    close() {}
  };
  const sockets = transport();
  let view;
  const client = connect('ws://localhost/acp', state => view?.show(state));
  view = mount(client);
  await new Promise(resolve => setImmediate(resolve));
  const select = document.querySelector('#agent');
  select.value = 'other';
  const input = document.querySelector('#prompt input');
  input.value = 'unsent draft';
  const sent = sockets[0].sent.length;
  agents = [{ id: 'new', name: 'New' }, { id: 'other', name: 'Renamed' }];
  source.revision({ data: String(++revision) });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(select.value, 'other');
  assert.equal(select.selectedOptions[0].textContent, 'Renamed');
  agents = [{ id: 'new', name: 'New' }];
  source.revision({ data: String(++revision) });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(select.value, 'new');
  assert.equal(select.options.length, 1);
  assert.equal(input.value, 'unsent draft');
  assert.equal(client.state().session.sessionId, 'hive-tui');
  assert.equal(sockets[0].sent.length, sent);
  view.close();
  dom.window.close();
});

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

test('the web shell uses the canonical embedded icon for its header and favicon', () => {
  const dom = new JSDOM(readFileSync(new URL('./index.html', import.meta.url), 'utf8'));
  const root = dom.window.document;
  assert.equal(root.querySelector('link[rel="icon"]').getAttribute('href'), '/ui/icon.svg');
  assert.equal(root.querySelector('header h1 img').getAttribute('src'), '/ui/icon.svg');
  assert.equal(root.querySelector('header h1 img').getAttribute('alt'), '');
  assert.equal(root.querySelector('#create').type, 'button');
  dom.window.close();
});

test('the web controls discover grouped providers and configure before enabling inference', async () => {
  const dom = new JSDOM(readFileSync(new URL('./index.html', import.meta.url), 'utf8'), { url: 'http://localhost/ui' });
  globalThis.document = dom.window.document;
  globalThis.location = dom.window.location;
  globalThis.history = dom.window.history;
  globalThis.FormData = dom.window.FormData;
  globalThis.fetch = async url => ({ ok: true, json: async () => url.includes('providers') ? { agents: [{ id: 'opencode', name: 'OpenCode' }] } : { revision: 1, sessions: [] } });
  globalThis.EventSource = class { addEventListener() {} close() {} };
  const sockets = transport((_, message, result) => {
    if (message.method === 'session/new') return { ...result, configOptions: modelOptions('fast') };
    if (message.method === 'session/set_config_option') return { configOptions: modelOptions(message.params.value) };
  });
  let view;
  const client = connect('ws://localhost/acp', state => view?.show(state));
  view = mount(client);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(document.querySelector('#send').disabled, true);
  document.querySelector('#agent').value = 'opencode';
  document.querySelector('#create').click();
  await new Promise(resolve => setImmediate(resolve));
  const select = document.querySelector('#configuration select');
  assert.equal(select.value, 'fast');
  assert.equal(select.querySelector('optgroup').label, 'Provider');
  assert.equal(select.disabled, false);
  select.value = 'careful';
  select.dispatchEvent(new dom.window.Event('change'));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(document.querySelector('#configuration select').value, 'careful');
  assert.equal(document.querySelector('#send').disabled, false);
  assert.deepEqual(sockets[0].sent.map(message => message.method), ['initialize', 'session/new', 'session/set_config_option']);
  view.close();
  dom.window.close();
});

test('browser and native form validation obey the same conformance cases', () => {
  const cases = JSON.parse(readFileSync(new URL('../../crates/hive-protocol/tests/fixtures/elicitation.json', import.meta.url)));
  for (const item of cases) {
    if (item.valid) assert.doesNotThrow(() => validateContent(item.schema, item.content), item.name);
    else assert.throws(() => validateContent(item.schema, item.content), undefined, item.name);
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

test('session UI lists daemon sessions and typing triggers handoff before send', async () => {
  const dom = new JSDOM(`<!doctype html><p id="status"></p><ul id="sessions"></ul>
    <form id="takeover"><input name="session"><button>Open</button></form><button id="skip-history"></button>
    <div id="output"></div><section id="interaction"></section>
    <form id="prompt"><select id="agent"><option value="">Default</option></select><input name="text"><button id="send">Send</button></form>
    <button id="retry"></button><button id="cancel"></button>`, { url: 'http://localhost/ui' });
  globalThis.document = dom.window.document;
  globalThis.location = dom.window.location;
  globalThis.history = dom.window.history;
  globalThis.FormData = dom.window.FormData;
  const requests = [];
  globalThis.fetch = async url => {
    requests.push(url);
    assert.ok(['/v1/providers', '/v1/sessions'].includes(url));
    return { ok: true, json: async () => url === '/v1/providers'
      ? { agents: [{ id: 'pi', name: 'Pi' }] } : { revision: 1, sessions: [session] } };
  };
  let eventsUrl;
  globalThis.EventSource = class {
    constructor(url) { eventsUrl = url; }
    addEventListener() {}
    close() {}
  };
  const sockets = transport();
  let view;
  const client = connect('ws://localhost/acp', state => view?.show(state));
  view = mount(client);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(requests.toSorted(), ['/v1/providers', '/v1/sessions']);
  assert.equal(eventsUrl, '/v1/sessions/events');
  assert.match(document.querySelector('#sessions').textContent, /pi · idle · controlled by another client/);
  document.querySelector('#sessions button').click();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().role, 'observer');
  assert.equal(document.querySelector('#send').disabled, true);
  const input = document.querySelector('#prompt input');
  input.value = 'follow up';
  input.dispatchEvent(new dom.window.Event('input', { bubbles: true }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().role, 'pilot');
  assert.equal(document.querySelector('#send').disabled, false);
  assert.equal(sockets[0].sent.filter(item => item.method === 'session/resume').length, 2);
  view.close();
  dom.window.close();
});

test('agent input UI collects typed form values and prevents stale interaction responses', async () => {
  const dom = new JSDOM(`<!doctype html><p id="status"></p><ul id="sessions"></ul>
    <form id="takeover"><input name="session"></form><button id="skip-history"></button><div id="output"></div>
    <section id="interaction"></section><form id="prompt"><select id="agent"></select><input name="text"><button id="send"></button></form>
    <button id="retry"></button><button id="cancel"></button>`, { url: 'http://localhost/' });
  globalThis.document = dom.window.document;
  globalThis.location = dom.window.location;
  globalThis.history = dom.window.history;
  globalThis.FormData = dom.window.FormData;
  globalThis.fetch = async url => ({ ok: true, json: async () => url.includes('providers')
    ? { agents: [] } : { revision: 1, sessions: [session] } });
  globalThis.EventSource = class { addEventListener() {} close() {} };
  const sockets = transport();
  let view;
  const client = connect('ws://localhost/acp', state => view?.show(state));
  view = mount(client);
  await new Promise(resolve => setImmediate(resolve));
  document.querySelector('#sessions button').click();
  await new Promise(resolve => setImmediate(resolve));
  const input = document.querySelector('#prompt input');
  input.value = 'take over';
  input.dispatchEvent(new dom.window.Event('input'));
  await new Promise(resolve => setImmediate(resolve));
  sockets[0].receive({ jsonrpc: '2.0', id: 50, method: 'elicitation/create', params: {
    sessionId: 'hive-tui', mode: 'form', message: 'Choose', requestedSchema: {
      properties: { approved: { type: 'boolean' }, count: { type: 'integer' } }, required: ['approved', 'count'],
    },
  } });
  const form = document.querySelector('#interaction form');
  form.elements.namedItem('approved').value = 'true';
  form.elements.namedItem('count').value = '4';
  form.dispatchEvent(new dom.window.Event('submit', { bubbles: true, cancelable: true }));
  assert.deepEqual(sockets[0].sent.at(-1).result, { action: 'accept', content: { approved: true, count: 4 } });
  sockets[0].receive({ jsonrpc: '2.0', id: 52, method: 'elicitation/create', params: {
    sessionId: 'hive-tui', mode: 'url', message: 'Authenticate', url: 'https://example.org/auth',
  } });
  assert.equal(document.querySelector('#interaction a').href, 'https://example.org/auth');
  assert.equal(document.querySelector('#interaction a').rel, 'noopener noreferrer');
  document.querySelector('#interaction button').click();
  assert.deepEqual(sockets[0].sent.at(-1).result, { action: 'accept' });
  sockets[0].receive({ jsonrpc: '2.0', id: 51, method: 'session/request_permission', params: {
    sessionId: 'hive-tui', title: 'Allow?', options: [{ name: 'Allow once', optionId: 'allow-once' }],
  } });
  assert.match(document.querySelector('#interaction').textContent, /Allow once/);
  sockets[0].receive(update(1, { sessionUpdate: '_hive_control', generation: 4, pilot: true }));
  assert.equal(document.querySelector('#interaction').hidden, true);
  assert.equal(client.respond({ outcome: { outcome: 'selected', optionId: 'allow-once' } }, 51), false);
  view.close();
  dom.window.close();
});

test('the session rail toggles, follows catalog revisions and keeps recent selection', async () => {
  const dom = new JSDOM(readFileSync(new URL('./index.html', import.meta.url), 'utf8'),
    { url: 'http://localhost/ui?session=hive-tui' });
  globalThis.document = dom.window.document;
  globalThis.location = dom.window.location;
  globalThis.history = dom.window.history;
  globalThis.FormData = dom.window.FormData;
  let revision = 1;
  let items = [{ ...session, lifecycle: 'active', generation: 3 }, { ...session, sessionId: 'hive-second',
    title: null, activity: 'unknown', controlOwner: null, lastActivityAt: null }];
  globalThis.fetch = async url => ({ ok: true, json: async () => url.includes('providers')
    ? { agents: [] } : { revision, sessions: items } });
  let source;
  globalThis.EventSource = class {
    constructor() { source = this; }
    addEventListener(_, listener) { this.revision = listener; }
    close() {}
  };
  const sockets = transport();
  let view;
  const client = connect('ws://localhost/acp', state => view?.show(state));
  view = mount(client);
  await new Promise(resolve => setImmediate(resolve));
  const toggle = document.querySelector('#toggle-sessions');
  assert.equal(client.state().session.sessionId, 'hive-tui');
  toggle.click();
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(document.querySelector('#layout').classList.contains('nav-hidden'), true);
  toggle.click();
  assert.equal(toggle.getAttribute('aria-expanded'), 'true');
  document.querySelector('[data-session-id="hive-second"]').click();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(client.state().session.sessionId, 'hive-second');
  assert.equal(location.pathname, '/ui');
  assert.equal(location.search, '?session=hive-second');
  assert.equal(document.querySelector('#sessions button').dataset.sessionId, 'hive-second');
  assert.equal(document.querySelector('#sessions [aria-current="page"]').dataset.sessionId, 'hive-second');
  revision = 2;
  items = items.map(item => item.sessionId === 'hive-second'
    ? { ...item, activity: 'running', controlOwner: 'other', lastActivityAt: Date.now() } : item);
  source.revision({ data: '2' });
  await new Promise(resolve => setImmediate(resolve));
  assert.match(document.querySelector('#sessions').textContent, /running · controlled by another client/);
  assert.equal(document.querySelector('#sessions [aria-current="page"]').dataset.sessionId, 'hive-second');
  assert.equal(sockets[0].sent.filter(message => message.method === 'session/new').length, 0);
  view.close();
  dom.window.close();
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
