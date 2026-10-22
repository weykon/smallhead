import assert from 'node:assert/strict';
import { test } from 'node:test';
import { apply } from './index.js';

function sendTool() {
  const tools = [];
  apply({
    get(name) { return name === 'tools' ? { register(tool) { tools.push(tool); } } : null; },
    effect(effect) { effect(); },
  });
  return tools.find((tool) => tool.name === 'smallhead_a2a_send');
}

test('A2A sends explicit development permissions and conversation context', async (t) => {
  let sent;
  t.mock.method(globalThis, 'fetch', async (_url, init) => {
    sent = JSON.parse(init.body);
    return Response.json({ status: 'done', text: 'verified', task_id: 'task', conversation_id: 'conv', errors: [] });
  });
  const reply = await sendTool().execute({ text: 'fix a bug', development: true, conversation_id: 'conv' });
  assert.equal(sent.permissions.can_mutate, true);
  assert.equal(sent.permissions.can_execute, true);
  assert.equal(sent.permissions.can_forward, false);
  assert.equal(sent.conversation_id, 'conv');
  assert.equal(reply.conversationId, 'conv');
  assert.equal(reply.ok, true);
});

test('A2A defaults to read-only and never treats a failed reply as success', async (t) => {
  let sent;
  t.mock.method(globalThis, 'fetch', async (_url, init) => {
    sent = JSON.parse(init.body);
    return Response.json({ status: 'failed', text: 'policy failed', errors: [] });
  });
  const reply = await sendTool().execute({ text: 'inspect' });
  assert.equal(sent.permissions.can_mutate, false);
  assert.equal(sent.permissions.can_execute, false);
  assert.equal(reply.ok, false);
});

test('A2A polls queued tasks without resubmitting the development request', async (t) => {
  const calls = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    calls.push({ url, method: init?.method || 'GET' });
    return Response.json(calls.length === 1
      ? { status: 'queued', task_id: 'task', errors: [] }
      : { status: 'done', task_id: 'task', conversation_id: 'conv', text: 'tested', errors: [] });
  });
  const reply = await sendTool().execute({ text: 'test a project', development: true });
  assert.equal(reply.ok, true);
  assert.equal(calls.length, 2);
  assert.equal(calls[0].method, 'POST');
  assert.equal(calls[1].method, 'GET');
  assert.ok(calls[1].url.endsWith('/a2a/tasks/task'));
});
