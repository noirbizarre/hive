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
        : message.method === 'session/new' ? { sessionId: 'hive-test' } : { messageId: 'user-test' } }) }));
    }
  };
  const updates = [];
  const client = connect('ws://127.0.0.1:8765/acp', update => updates.push(update), () => {});
  await client.prompt('hello');
  assert.deepEqual(sent.map(message => message.method), ['initialize', 'session/new', 'session/prompt']);
  assert.equal(sent[2].params.sessionId, 'hive-test');
  socket.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'someone-else', update: { sessionUpdate: 'agent_message_chunk' } } }) });
  socket.onmessage({ data: JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'hive-test', update: { sessionUpdate: 'agent_message_chunk' } } }) });
  assert.equal(updates.length, 1);
  client.cancel();
  assert.equal(sent[3].method, 'session/cancel');
  globalThis.WebSocket = undefined;
});
