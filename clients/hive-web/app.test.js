import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { connect } from './app.js';

test('browser initializes with ACPv2 before sending a prompt', async () => {
  const sent = [];
  globalThis.WebSocket = class {
    static OPEN = 1;
    readyState = 1;
    constructor() { globalThis.socket = this; queueMicrotask(() => this.onopen()); }
    send(raw) {
      const message = JSON.parse(raw);
      sent.push(message);
      queueMicrotask(() => this.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', id: message.id, result: message.method === 'initialize'
        ? { protocolVersion: 2, capabilities: { session: {} } }
        : message.method === 'session/new' ? { sessionId: 'hive-test', _meta: { hive: { generation: 1, token: 'owner' } } } : { messageId: 'user-test' } }) }));
    }
  };
  const updates = [];
  const client = connect('ws://127.0.0.1:8765/acp', update => updates.push(update), () => {});
  await client.prompt('hello', 'pi');
  assert.deepEqual(sent.map(message => message.method), ['initialize', 'session/new', 'session/prompt']);
  assert.equal(sent[2].params.sessionId, 'hive-test');
  assert.equal(sent[1].params._meta.hive.agent, 'pi');
  assert.equal(sent[2].params._meta.hive.token, 'owner');
  assert.ok(sent[2].params._meta.hive.key);
  socket.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'someone-else', update: { sessionUpdate: 'agent_message_chunk' } } }) });
  socket.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'hive-test', update: { sessionUpdate: 'agent_message_chunk' } } }) });
  assert.equal(updates.length, 1);
  client.cancel();
  assert.equal(sent[3].method, 'session/cancel');
  assert.equal(sent[3].params._meta.hive.generation, 1);
  globalThis.WebSocket = undefined;
});

test('browser takes over an existing session and fences commands with the new token', async () => {
  const values = new Map([['hive.session', 'hive-existing'], ['hive.seq.hive-existing', '7']]);
  globalThis.localStorage = {
    getItem: key => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: key => values.delete(key),
  };
  const sent = [];
  globalThis.WebSocket = class {
    static OPEN = 1;
    readyState = 1;
    constructor() { queueMicrotask(() => this.onopen()); }
    send(raw) {
      const message = JSON.parse(raw);
      sent.push(message);
      const result = message.method === 'initialize' ? { protocolVersion: 2, capabilities: { session: {} } }
        : message.method === 'session/resume' ? { _meta: { hive: { generation: 4, token: 'new-owner', running: false } } }
          : { messageId: 'user-1' };
      queueMicrotask(() => this.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', id: message.id, result }) }));
    }
  };
  const updates = [];
  const client = connect('ws://127.0.0.1/acp', update => updates.push(update), () => {});
  await client.prompt('after handoff');
  assert.deepEqual(sent.map(message => message.method), ['initialize', 'session/resume', 'session/prompt']);
  assert.deepEqual(sent[1].params.replayFrom, { type: '_hive_seq', seq: 8 });
  assert.equal(sent[2].params._meta.hive.token, 'new-owner');
  assert.equal(sent[2].params._meta.hive.generation, 4);
  globalThis.localStorage = undefined;
  globalThis.WebSocket = undefined;
});
