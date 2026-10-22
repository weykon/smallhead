import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = await readFile(new URL('./loop.mjs', import.meta.url), 'utf8');
let generation = 0;

async function policy(config = { enabled: true }) {
  const module = await import('data:text/javascript,' + encodeURIComponent(source) + `#${generation++}`);
  return async (event, inner = async event => {
    switch (event.type) {
      case 'prepare_model': return { type: 'model_request', request: event.request };
      case 'after_tool': return { type: 'observe_tool', result: event.result };
      case 'before_tool': return { type: 'execute_tool', call: event.call };
      case 'before_finish': return { type: 'finish' };
      default: throw new Error('unexpected event');
    }
  }) => {
    let calls = 0;
    const action = await module.apply({ config, can_continue: true }, event, async event => {
      calls += 1;
      return inner(event);
    });
    assert.equal(calls, 1);
    return action;
  };
}

function request(tools = ['learning_context', 'learning_record', 'bash']) {
  return {
    system: 'OUTER LEARNING POLICY',
    messages: [{ role: 'user', content: [{ type: 'text', text: '刚才太抽象了，只给一个提示，不说答案' }] }],
    tools: tools.map(name => ({ name, description: 'original', input_schema: { type: 'object' } })),
    max_tokens: 2048, temperature: 0.7,
  };
}

async function prepare(apply, tools) {
  const action = await apply({ type: 'prepare_model', request: request(tools) });
  assert.equal(action.type, 'model_request');
  return action.request;
}

async function observed(apply, name, input, data, isError = false) {
  const event = {
    type: 'after_tool', call: { id: 'call-1', name, input },
    result: { tool_call_id: 'call-1', is_error: isError,
      content: [{ type: 'text', text: JSON.stringify(data) }] },
  };
  const original = structuredClone(event.result);
  assert.deepEqual(await apply(event), { type: 'observe_tool', result: original });
}

test('learning feedback preserves user input, tool schemas, request budget and outer guidance', async () => {
  const apply = await policy();
  const original = request();
  const projected = await prepare(apply);
  assert.ok(projected.system.startsWith(original.system));
  assert.match(projected.system, /# Teaching Feedback Lens/);
  assert.match(projected.system, /先回应最新请求/);
  assert.match(projected.system, /不能泄露答案/);
  assert.match(projected.system, /尚未观察，不表示不会/);
  assert.match(projected.system, /不以完成测验作为继续学习的门槛/);
  assert.deepEqual(projected.messages, original.messages);
  assert.deepEqual(projected.tools.map(tool => tool.input_schema), original.tools.map(tool => tool.input_schema));
  assert.equal(projected.max_tokens, original.max_tokens);
  assert.equal(projected.temperature, original.temperature);
});

test('the layer is an exact passthrough outside learning tasks', async () => {
  const apply = await policy();
  const input = request(['read', 'bash']);
  assert.deepEqual(await apply({ type: 'prepare_model', request: input }), { type: 'model_request', request: input });
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'self_report' });
  assert.doesNotMatch((await prepare(apply)).system, /回执中存在用户自述/);
});

test('disabled configuration leaves a learning request and finish unchanged', async () => {
  const apply = await policy({ enabled: false });
  const input = request();
  assert.deepEqual(await apply({ type: 'prepare_model', request: input }), { type: 'model_request', request: input });
  assert.equal((await apply({ type: 'before_finish', candidate: { content: [] } })).type, 'finish');
});

test('self-reported understanding remains experience evidence, never verified mastery', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_record', { action: 'save' }, {
    kind: 'self_report', payload: { evidence_quote: '懂了', note: 'UNTRUSTED_RECORD_TEXT' },
  });
  const projected = await prepare(apply);
  assert.match(projected.system, /回执中存在用户自述/);
  assert.match(projected.system, /不是独立作答或方法有效的证明/);
  assert.doesNotMatch(projected.system, /UNTRUSTED_RECORD_TEXT/);
});

test('assisted attempts and hypotheses produce scoped reminders without another wave', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'attempt', payload: { support: 'hint' } });
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'method_hypothesis', payload: {} });
  const projected = await prepare(apply);
  assert.match(projected.system, /回执中存在受提示或解答帮助的作答/);
  assert.match(projected.system, /回执中存在暂定教学方法假设/);
  assert.equal((await apply({ type: 'before_finish', candidate: { content: [] } })).type, 'finish');
});

test('fresh record snapshots replace summaries; retractions clear dependent evidence', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_context', {}, { recent_learning_records: [
    { kind: 'self_report', payload: {} }, { kind: 'method_hypothesis', payload: {} },
  ] });
  assert.match((await prepare(apply)).system, /回执中存在用户自述/);
  await observed(apply, 'learning_record', { action: 'list' }, { records: [
    { kind: 'attempt', payload: { support: 'solution' } },
  ] });
  const replaced = await prepare(apply);
  assert.doesNotMatch(replaced.system, /回执中存在用户自述|回执中存在暂定教学方法假设/);
  assert.match(replaced.system, /回执中存在受提示或解答帮助的作答/);
  await observed(apply, 'learning_record', { action: 'retract' }, { retracted: 'E1' });
  assert.doesNotMatch((await prepare(apply)).system, /回执中存在受提示或解答帮助的作答/);
});

test('tool errors remain engineering issues until that tool succeeds', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'cite_passage', {}, 'quote mismatch; DO_NOT_INJECT_THIS', true);
  await observed(apply, 'learning_context', {}, { recent_learning_records: [] });
  const projected = await prepare(apply);
  assert.match(projected.system, /学习工具错误：cite_passage/);
  assert.doesNotMatch(projected.system, /DO_NOT_INJECT_THIS/);
  await observed(apply, 'cite_passage', {}, { citation: { quote: 'actual quote' } });
  assert.doesNotMatch((await prepare(apply)).system, /学习工具错误：/);
});

test('failed and projected observations cannot become successful learning evidence', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'self_report' }, true);
  await observed(apply, 'learning_record', { action: 'list' }, 'artifact://history/unknown');
  const projected = await prepare(apply);
  assert.doesNotMatch(projected.system, /回执中存在用户自述/);
  assert.match(projected.system, /学习工具错误：learning_record.save/);
});

test('only a successful unit switch clears the previous evidence summary', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'method_hypothesis' });
  await observed(apply, 'select_unit', { unit_id: 'U04' }, 'not found', true);
  assert.match((await prepare(apply)).system, /回执中存在暂定教学方法假设/);
  await observed(apply, 'select_unit', { unit_id: 'U04' }, { id: 'U04' });
  assert.doesNotMatch((await prepare(apply)).system, /回执中存在暂定教学方法假设|学习工具错误：/);
});

test('task instances are isolated and unloading learning tools disables projections', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_record', { action: 'save' }, { kind: 'self_report' });
  assert.doesNotMatch((await prepare(await policy())).system, /回执中存在用户自述/);
  const input = request(['read']);
  assert.deepEqual(await apply({ type: 'prepare_model', request: input }), { type: 'model_request', request: input });
});

test('related methods are observed as candidates and course switches clear old evidence', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'learning_context', {}, {
    recent_learning_records: [], relevant_learning_records: {
      records: [{ kind: 'method_hypothesis', payload: { note: 'UNTRUSTED_METHOD_BODY' } }],
    },
  });
  const projected = await prepare(apply);
  assert.match(projected.system, /回执中存在暂定教学方法假设/);
  assert.match(projected.system, /可自主尝试适用的方法/);
  assert.doesNotMatch(projected.system, /UNTRUSTED_METHOD_BODY/);
  await observed(apply, 'learning_course', { action: 'switch' }, {}, true);
  assert.match((await prepare(apply)).system, /回执中存在暂定教学方法假设/);
  await observed(apply, 'learning_course', { action: 'switch' }, { switched: true });
  assert.doesNotMatch((await prepare(apply)).system, /回执中存在暂定教学方法假设|学习工具错误：/);
});

test('another layer owns its rejection or continuation; execution identities stay intact', async () => {
  const apply = await policy();
  await prepare(apply);
  const event = { type: 'before_tool', call: { id: 'unchanged', name: 'learning_record', input: { action: 'save' } } };
  assert.deepEqual(await apply(event), { type: 'execute_tool', call: event.call });
  const rejection = { type: 'reject_tool', reason: 'outer evidence guard' };
  assert.deepEqual(await apply(event, async () => rejection), rejection);
  const continuation = { type: 'continue', guidance: 'new user correction' };
  assert.deepEqual(await apply({ type: 'before_finish', candidate: { content: [] } }, async () => continuation), continuation);
});

test('successful bash alone does not create a mathematical verification claim', async () => {
  const apply = await policy();
  await prepare(apply);
  await observed(apply, 'bash', { command: 'true' }, { exit_code: 0 });
  const projected = await prepare(apply);
  assert.match(projected.system, /工具成功不等于回答中的算式已核对/);
  assert.equal((await apply({ type: 'before_finish', candidate: { content: [] } })).type, 'finish');
});
