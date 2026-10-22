import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const directory = process.argv[2];
assert(directory, 'usage: node scripts/verify-monad-live.mjs EVIDENCE_DIRECTORY');
const read = name => JSON.parse(fs.readFileSync(path.join(directory, name), 'utf8'));
const events = fs.readFileSync(path.join(directory, 'events.jsonl'), 'utf8')
  .trim().split('\n').filter(Boolean).map(JSON.parse);
const runs = read('runs.json');
const outcome = read('outcome.json');
const inputs = fs.readFileSync(path.join(directory, 'inputs.jsonl'), 'utf8')
  .trim().split('\n').filter(Boolean).map(JSON.parse);
const text = messages => (messages ?? []).flatMap(message => message.content ?? [])
  .map(block => block.text ?? '').join('\n');
const calls = events.filter(event => event.layer === 'tool' && event.event === 'execute_start');
const ends = new Map(events.filter(event => event.layer === 'tool' && event.event === 'execute_end')
  .map(event => [event.data.id, event.data.result]));
const accepted = action => calls.filter(event => event.data.name === 'monad_delegate'
  && event.data.input.action === action && ends.get(event.data.id)?.is_error === false);
const pause = accepted('pause')[0];
const resume = accepted('resume')[0];
assert(pause && resume, 'missing successful parent pause/resume');
const ui = pause.data.input.task_id;
const service = runs.find(run => run.handoff.task_id !== ui);
assert(service, 'missing independent service child');
const visible = new Map();
let pauseObserved;
for (const event of events) {
  if (event.layer !== 'request' || event.event !== 'provider_request') continue;
  for (const line of text(event.data.request.messages).split('\n')) {
    let payload;
    try { payload = JSON.parse(line); } catch { continue; }
    if (!payload.childtask || !payload.kind) continue;
    visible.set(payload.seq, payload);
    if (payload.childtask === ui && payload.kind === 'paused') pauseObserved ??= event.timestamp_ms;
  }
}
const serviceSend = accepted('send').find(event => event.data.input.task_id === service.handoff.task_id);
const uiSend = accepted('send').find(event => event.data.input.task_id === ui);
assert(serviceSend && uiSend, 'missing successful requirement/fact delivery');
const childRequests = id => events.filter(event => event.event === 'child_event'
  && event.data.task_id === id && event.data.layer === 'request'
  && event.data.event === 'provider_request');
const delivered = (id, message) => childRequests(id)
  .some(event => text(event.data.data.request.messages).includes(message));
const uiToolsDuringPause = events.filter(event => event.event === 'child_event'
  && event.data.task_id === ui && event.data.layer === 'tool' && event.data.event === 'execute_start'
  && event.timestamp_ms >= pauseObserved && event.timestamp_ms < resume.timestamp_ms);
const findings = [...visible.values()].filter(event => event.childtask === service.handoff.task_id
  && event.kind.report === 'finding').map(event => event.text).join('\n');
const sharedFacts = ['external', 'kill_on_drop', 'StreamableHttpClientTransport'];
const checks = {
  real_model: outcome.model === 'MiniMax-M3',
  all_three_inputs_in_one_turn: inputs.length === 3 && outcome.change_sent && outcome.resume_sent,
  parent_and_two_children_completed: Boolean(outcome.result.Ok) && runs.length === 2
    && runs.every(run => run.state === 'Completed'),
  identities_match_parent: runs.every(run => run.handoff.parent_task_id === outcome.runtime_parent),
  pause_acknowledged: Number.isFinite(pauseObserved) && outcome.pause_acknowledged,
  no_child_tool_started_while_paused: Number.isFinite(pauseObserved) && uiToolsDuringPause.length === 0,
  new_requirement_entered_service_context: delivered(service.handoff.task_id, serviceSend.data.input.message),
  source_facts_reported_before_forwarding: sharedFacts.every(fact => findings.includes(fact)
    && uiSend.data.input.message.includes(fact)) && service.result.finished_at_ms <= uiSend.timestamp_ms,
  forwarded_facts_entered_ui_context: delivered(ui, uiSend.data.input.message),
  facts_sent_before_resume: uiSend.timestamp_ms <= resume.timestamp_ms,
  both_terminal_reports_visible: runs.every(run => [...visible.values()].some(event =>
    event.childtask === run.handoff.task_id && event.kind === 'ended'
    && event.runtime?.state === 'Completed' && event.runtime.run_id === run.run_id)),
};
const summary = {
  checks,
  elapsed_seconds: outcome.result.Ok?.usage?.elapsed_ms / 1000,
  child_usage: runs.map(run => ({ task_id: run.handoff.task_id, ...run.result.usage })),
  parent_tool_errors: calls.filter(call => ends.get(call.data.id)?.is_error)
    .map(call => ({ tool: call.data.name, action: call.data.input.action, error: ends.get(call.data.id).preview })),
  content_correctness: 'Requires independent source review; completion and message delivery do not prove claims.',
};
console.log(JSON.stringify(summary, null, 2));
fs.writeFileSync(path.join(directory, 'verification.json'), JSON.stringify(summary, null, 2) + '\n');
assert(Object.values(checks).every(Boolean), 'collaboration evidence failed one or more checks');
