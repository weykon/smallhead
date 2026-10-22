import readline from 'node:readline';
import { format } from 'node:util';

// stdout is exclusively the host protocol, including during module import.
for (const key of ['log', 'info', 'warn', 'error', 'debug']) {
  console[key] = (...args) => process.stderr.write(format(...args) + '\n');
}
const send = value => process.stdout.write(JSON.stringify(value) + '\n');
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let apply;
let config;
let pending;

input.on('line', line => {
  let message;
  try { message = JSON.parse(line); }
  catch { process.exit(2); }
  if (message.type === 'next_result') {
    if (!pending || pending.id !== message.id || !pending.resolve) process.exit(3);
    const { resolve, reject } = pending;
    pending.resolve = null;
    pending.reject = null;
    message.error ? reject(new Error(message.error)) : resolve(message.action);
    return;
  }
  if (message.type === 'initialize' && !apply) {
    (async () => {
      config = message.config;
      const module = await import('data:text/javascript,' + encodeURIComponent(message.source));
      if (typeof module.apply !== 'function') throw new Error('Lens must export apply(context, event, next)');
      apply = module.apply;
      send({ type: 'ready' });
    })().catch(error => send({ type: 'error', error: String(error.stack || error) }));
    return;
  }
  if (message.type !== 'evaluate' || !apply || pending) process.exit(4);
  pending = { id: message.id, used: false, violated: false };
  const current = pending;
  const next = event => {
    if (current !== pending || current.used) {
      current.violated = true;
      return Promise.reject(new Error('next is single-use within its checkpoint'));
    }
    current.used = true;
    return new Promise((resolve, reject) => {
      current.resolve = resolve;
      current.reject = reject;
      send({ type: 'next', id: message.id, event });
    });
  };
  (async () => {
    const action = await apply(Object.freeze({ ...message.context, config }), message.event, next);
    if (current.violated || current.resolve) throw new Error('next must be awaited exactly once when used');
    send({ type: 'result', id: message.id, action });
  })().catch(error => send({ type: 'error', id: message.id, error: String(error.stack || error) }))
    .finally(() => { pending = null; });
});
input.on('close', () => process.exit(0));
