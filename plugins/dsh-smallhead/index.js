/**
 * Host half of `@local/dsh-smallhead`.
 *
 * Read-only observer for a local `smallhead` workspace:
 *
 * - serves `GET /smallhead/api/sessions` (session list from `index.json`) and
 *   `GET /smallhead/api/session?id=<uuid-v7>` (one session snapshot, trimmed), so
 *   the Client panel renders without a Host RPC of its own;
 * - registers the `smallhead_status` agent tool over the same two reads, so the
 *   panel and the agent never diverge: one operation, two callers.
 *
 * Session inspection never writes to the store. A2A sends requests to a running
 * smallhead-server; development capabilities are opt-in per request and must
 * also be enabled by that local server.
 *
 * @module @local/dsh-smallhead
 */
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';

/** Loader row services this plugin needs before it activates. */
export const inject = ['webServer', 'tools'];

/** Fixed, package-owned route prefix; the Client half reads the same constant. */
const API_PREFIX = '/smallhead/api';

/** Session ids are uuid-v7 strings; anything else must never reach a path join. */
const SESSION_ID_PATTERN = /^[0-9a-fA-F-]{8,64}$/;

/**
 * One row of `<sessionsDir>/index.json`, as `smallhead-session`'s `JsonFileStore`
 * writes it.
 * @typedef {object} SessionMeta
 * @property {string} id
 * @property {number} createdAt epoch millis
 * @property {number} updatedAt epoch millis
 * @property {number} messageCount
 * @property {string} preview first user message, may be empty
 */

/**
 * Row config fields, all optional and read defensively:
 * `sessionsDir` (defaults to `~/.smallhead/sessions`), `statusToolName`
 * (defaults to `smallhead_status`), `defaultLimit` (20), `maxLimit` (200).
 *
 * The row declares no `Config` schema: this package is linked into the profile
 * from outside it, so a harness import would not resolve from the bundle
 * directory. Unknown fields are ignored instead of validated.
 */

/**
 * Resolve the session store directory from config and the environment.
 * @param {unknown} configured - `config.sessionsDir`, empty when unset.
 * @returns {string} absolute directory path.
 */
function resolveSessionsDir(configured) {
  if (typeof configured === 'string' && configured.trim() !== '') return configured.trim();
  return join(homedir() || process.env.HOME || '.', '.smallhead', 'sessions');
}

/**
 * Clamp a caller-supplied limit into a configured range.
 * @param {unknown} value - raw caller value.
 * @param {number} fallback - value used when absent or unusable.
 * @param {number} max - hard upper bound.
 * @returns {number} a positive integer no greater than `max`.
 */
function clampLimit(value, fallback, max) {
  const ceiling = Math.max(1, Math.floor(max));
  const raw = Number(value);
  if (!Number.isFinite(raw) || raw <= 0) return Math.max(1, Math.min(Math.floor(fallback), ceiling));
  return Math.max(1, Math.min(Math.floor(raw), ceiling));
}

/**
 * Flatten one stored `AgentMessage`'s content blocks into display text.
 * @param {unknown} message - one stored message.
 * @returns {string} concatenated text.
 */
function messageText(message) {
  if (!message || typeof message !== 'object') return '';
  const content = message.content;
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  const parts = [];
  for (const block of content) {
    if (!block || typeof block !== 'object') continue;
    if (typeof block.text === 'string') parts.push(block.text);
    else if (typeof block.type === 'string') parts.push(`[${block.type}]`);
  }
  return parts.join(' ').trim();
}

/**
 * Read `index.json`. A missing index is an empty list rather than a failure,
 * because a workspace with no finished turn legitimately has none yet.
 * @param {string} dir - session store directory.
 * @returns {Promise<{ sessions: SessionMeta[], note: string }>} sorted sessions plus a diagnostic note.
 */
async function readIndex(dir) {
  try {
    const text = await readFile(join(dir, 'index.json'), 'utf8');
    if (text.trim() === '') return { sessions: [], note: '' };
    const parsed = JSON.parse(text);
    if (!Array.isArray(parsed)) return { sessions: [], note: 'index.json is not an array' };
    const sessions = parsed
      .filter((row) => row && typeof row === 'object' && typeof row.id === 'string')
      .map((row) => ({
        id: String(row.id),
        createdAt: Number(row.created_at) || 0,
        updatedAt: Number(row.updated_at) || 0,
        messageCount: Number(row.message_count) || 0,
        preview: typeof row.first_user_message === 'string' ? row.first_user_message : '',
      }))
      .sort((a, b) => b.updatedAt - a.updatedAt);
    return { sessions, note: '' };
  } catch (error) {
    if (error && error.code === 'ENOENT') return { sessions: [], note: '' };
    const reason = String(error && error.message ? error.message : error);
    return { sessions: [], note: `index.json unreadable: ${reason}` };
  }
}

/**
 * Read one session snapshot, trimmed to what a viewer needs: metadata plus a
 * bounded tail of short message previews instead of the full transcript.
 * @param {string} dir - session store directory.
 * @param {string} id - session id, already validated.
 * @param {number} previewMessages - how many trailing messages to preview.
 * @returns {Promise<object>} canonical detail value.
 */
async function readSession(dir, id, previewMessages) {
  const text = await readFile(join(dir, `${id}.json`), 'utf8');
  const parsed = JSON.parse(text);
  const messages = Array.isArray(parsed.messages) ? parsed.messages : [];
  const tail = messages.slice(Math.max(0, messages.length - previewMessages));
  return {
    session: {
      id: String(parsed.id ?? id),
      createdAt: Number(parsed.created_at) || 0,
      updatedAt: Number(parsed.updated_at) || 0,
      messageCount: messages.length,
      systemPromptLength: typeof parsed.system_prompt === 'string' ? parsed.system_prompt.length : 0,
      messages: tail.map((message) => ({
        role: String(message && message.role ? message.role : 'unknown'),
        text: messageText(message).slice(0, 400),
      })),
    },
  };
}

/**
 * Write one JSON response, owning the whole response lifecycle.
 * @param {import('node:http').ServerResponse} res - response.
 * @param {number} status - HTTP status.
 * @param {unknown} body - JSON-serializable body.
 */
function sendJson(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'content-length': Buffer.byteLength(payload),
  });
  res.end(payload);
}

/** Default smallhead A2A bridge address (loopback). */
const A2A_DEFAULT_PEER = 'http://127.0.0.1:3999';

/**
 * How long an inbound A2A request waits inline for its turn to settle. A faster
 * turn answers 200 with the reply; a slower one answers 202 `queued` and the
 * caller polls `/a2a/tasks/:id`, exactly as smallhead's own bridge does.
 */
const A2A_INLINE_WAIT_MS = 60_000;

/**
 * Submit one message to a smallhead A2A bridge and settle its task.
 *
 * Mirrors `smallhead-server send`: POST `/a2a/messages`, then poll
 * `/a2a/tasks/{task_id}` when the bridge answered `queued`. The reply shape is
 * canonical for both the panel route and the agent tool, so neither diverges.
 * @param {string} peer - bridge base URL, e.g. http://127.0.0.1:3999
 * @param {string} text - message text.
 * @param {string} intent - optional intent label.
 * @param {number} timeoutMs - poll budget.
 * @returns {Promise<object>} canonical outcome.
 */
async function a2aSend(peer, text, intent = 'research', timeoutMs = 120_000, options = {}) {
  const base = (peer || A2A_DEFAULT_PEER).replace(/\/+$/, '');
  try {
    const response = await fetch(`${base}/a2a/messages`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        from: 'dsh',
        to: 'smallhead',
        kind: 'request',
        intent,
        text,
        conversation_id: typeof options.conversation_id === 'string' ? options.conversation_id : null,
        permissions: {
          can_observe: true,
          can_plan: true,
          can_mutate: options.development === true,
          can_execute: options.development === true,
          can_forward: false,
        },
        ttl: 3,
      }),
    });
    if (!response.ok) {
      return { ok: false, status: 'error', text: `A2A bridge HTTP ${response.status}`, taskId: '', errors: [`HTTP ${response.status}`] };
    }
    let reply = await response.json();
    // The inline POST may itself wait 120s. Give a queued task a fresh poll
    // budget rather than consuming that budget while waiting for the POST.
    const deadline = Date.now() + timeoutMs;
    while (reply && reply.status === 'queued' && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 500));
      const poll = await fetch(`${base}/a2a/tasks/${reply.task_id}`);
      if (!poll.ok) break;
      reply = await poll.json();
    }
    return {
      ok: ['done', 'completed'].includes(reply.status) && (!Array.isArray(reply.errors) || reply.errors.length === 0),
      status: reply.status ?? 'unknown',
      text: reply.text ?? '',
      taskId: reply.task_id ?? '',
      conversationId: reply.conversation_id ?? '',
      errors: Array.isArray(reply.errors) ? reply.errors : [],
    };
  } catch (error) {
    return { ok: false, status: 'error', text: String(error && error.message ? error.message : error), taskId: '', errors: [String(error && error.message ? error.message : error)] };
  }
}

/**
 * Validate one inbound A2A message against the same rules smallhead applies.
 * Read-only first: any mutate/execute/forward permission is rejected.
 * @param {object} message - inbound A2A message.
 * @returns {string} '' when valid, otherwise a rejection reason.
 */
function validateInboundA2a(message) {
  if (!message || typeof message !== 'object') return 'invalid message';
  for (const field of ['from', 'kind']) {
    const value = typeof message[field] === 'string' ? message[field] : '';
    if (value.trim() === '' || value.length > 128) return `invalid A2A ${field}`;
  }
  const to = message.to;
  if (to !== undefined && to !== 'dsh' && to !== '*') return `A2A target is not this agent: ${to}`;
  const text = typeof message.text === 'string' ? message.text : '';
  if (text.trim() === '' || text.length > 16 * 1024) return 'A2A text must be 1..16384 bytes';
  const permissions = message.permissions || {};
  if (permissions.can_mutate || permissions.can_execute || permissions.can_forward) {
    return 'this local bridge only permits observe, research, plan, and reply';
  }
  return '';
}

/**
 * Pick a subagent provider name from the live registry, preferring the local
 * in-process backends.
 * @param {object} subagents - the subagents service.
 * @returns {string | undefined} a registered provider name.
 */
function pickSubagentProvider(subagents) {
  let names = [];
  try {
    names = typeof subagents.list === 'function' ? subagents.list() : [];
  } catch {
    names = [];
  }
  if (names.includes('fork')) return 'fork';
  if (names.includes('spawn')) return 'spawn';
  return names[0];
}

/**
 * Run one DSH turn for an inbound A2A message via the subagents service, and
 * return a canonical outcome. This is the receive half of the bidirectional
 * link: smallhead sends here, the DSH agent answers.
 * @param {object} subagents - the subagents service.
 * @param {object} agents - the agents service.
 * @param {string} text - message text to run.
 * @returns {Promise<object>} canonical outcome.
 */
async function runDshTurn(subagents, agents, text) {
  if (!subagents) return { ok: false, status: 'error', text: 'subagents service unavailable', taskId: '', errors: ['subagents service unavailable'] };
  const provider = pickSubagentProvider(subagents);
  if (!provider) return { ok: false, status: 'error', text: 'no subagent provider registered', taskId: '', errors: ['no subagent provider registered'] };

  let parent;
  try {
    parent =
      (typeof agents.currentInitiator === 'function' && agents.currentInitiator()) ||
      (Array.isArray(agents.roots && agents.roots()) && agents.roots()[0]) ||
      (Array.isArray(agents.list && agents.list()) && agents.list()[0]);
  } catch {
    parent = undefined;
  }
  if (!parent) return { ok: false, status: 'error', text: 'no live DSH agent to run a turn', taskId: '', errors: ['no live DSH agent'] };

  const controller = new AbortController();
  try {
    // DSH 0.2.1 replaced `subagents.start()` with `startActivation()`, and moved
    // child tool scoping from the removed `toolFilter` to Agent Presets. Keep a
    // fallback to `start` so the plugin still runs on 0.2.0.
    const request = { prompt: [{ type: 'text', text }], parent };
    let activation;
    if (typeof subagents.startActivation === 'function') {
      activation = await subagents.startActivation({
        provider,
        label: 'smallhead A2A inbound',
        request,
        signal: controller.signal,
        delivery: 'caller',
      });
    } else if (typeof subagents.start === 'function') {
      activation = await subagents.start(provider, {
        label: 'smallhead A2A inbound',
        prompt: request.prompt,
        parent,
        signal: controller.signal,
      });
    } else {
      throw new Error('subagents service exposes neither startActivation nor start');
    }
    let result;
    try {
      result = await activation.result;
    } finally {
      try {
        await activation.dispose();
      } catch {
        /* best-effort */
      }
    }
    const replyText = (Array.isArray(result && result.output) ? result.output : [])
      .filter((block) => block && block.type === 'text')
      .map((block) => block.text)
      .join('\n');
    return {
      ok: result && result.stopReason === 'completed',
      status: result && result.stopReason ? result.stopReason : 'unknown',
      text: replyText,
      taskId: '',
      errors: result && result.diagnostic ? [result.diagnostic] : [],
    };
  } catch (error) {
    return { ok: false, status: 'error', text: String(error && error.message ? error.message : error), taskId: '', errors: [String(error && error.message ? error.message : error)] };
  }
}

/**
 * Host plugin body: one route pair and one agent tool over two shared reads.
 * @param {import('@deepseek-ai/cordis').Context} ctx - plugin context.
 * @param {{ sessionsDir?: string, statusToolName?: string, defaultLimit?: number, maxLimit?: number }} [config] - row config.
 */
export function apply(ctx, config = {}) {
  const sessionsDir = resolveSessionsDir(config.sessionsDir);
  const defaultLimit = clampLimit(config.defaultLimit, 20, 1000);
  const maxLimit = clampLimit(config.maxLimit, 200, 5000);
  const toolName = config.statusToolName || 'smallhead_status';
  const a2aPeer = config.a2aPeer || A2A_DEFAULT_PEER;

  /**
   * List recent sessions, newest first.
   * @param {unknown} limit - caller page size.
   * @returns {Promise<object>} canonical list value.
   */
  async function listSessions(limit) {
    const { sessions, note } = await readIndex(sessionsDir);
    return {
      sessionsDir,
      total: sessions.length,
      sessions: sessions.slice(0, clampLimit(limit, defaultLimit, maxLimit)),
      unavailable: note === '' ? null : { reason: note },
    };
  }

  /**
   * Read one session's trimmed detail.
   * @param {unknown} id - session id.
   * @param {unknown} previewMessages - trailing message count.
   * @returns {Promise<object>} canonical detail value.
   */
  async function sessionDetail(id, previewMessages) {
    if (typeof id !== 'string' || !SESSION_ID_PATTERN.test(id)) {
      return { unavailable: { reason: 'sessionId must be a uuid-like string' } };
    }
    try {
      return await readSession(sessionsDir, id, clampLimit(previewMessages, 8, 60));
    } catch (error) {
      const reason =
        error && error.code === 'ENOENT'
          ? `no session file for ${id}`
          : String(error && error.message ? error.message : error);
      return { unavailable: { reason } };
    }
  }

  const webServer = ctx.get('webServer');
  if (webServer) {
    ctx.effect(
      () =>
        webServer.register({
          kind: 'prefix',
          path: API_PREFIX,
          handler: async (req, res) => {
            try {
              const url = new URL(req.url ?? '/', 'http://127.0.0.1');
              const rest = url.pathname.slice(API_PREFIX.length);

              // A2A send (write path): both the panel composer and the agent tool
              // land here, so the operation lives in exactly one place.
              if (req.method === 'POST' && rest === '/a2a/send') {
                const chunks = [];
                for await (const chunk of req) chunks.push(chunk);
                const body = Buffer.concat(chunks).toString('utf8');
                let input = {};
                try {
                  input = body === '' ? {} : JSON.parse(body);
                } catch {
                  sendJson(res, 400, { error: 'invalid-json' });
                  return;
                }
                const text = typeof input.text === 'string' ? input.text : '';
                if (text.trim() === '') {
                  sendJson(res, 400, { error: 'text is required' });
                  return;
                }
                sendJson(res, 200, await a2aSend(a2aPeer, text, input.intent, 120_000, input));
                return;
              }

              if (req.method !== 'GET') {
                sendJson(res, 405, { error: 'method-not-allowed' });
                return;
              }
              if (rest === '' || rest === '/' || rest === '/sessions') {
                sendJson(res, 200, await listSessions(url.searchParams.get('limit')));
                return;
              }
              if (rest === '/session') {
                sendJson(res, 200, await sessionDetail(url.searchParams.get('id'), url.searchParams.get('preview')));
                return;
              }
              sendJson(res, 404, { error: 'not-found', prefix: API_PREFIX });
            } catch (error) {
              sendJson(res, 500, { error: String(error && error.message ? error.message : error) });
            }
          },
        }),
      'smallhead: read-only routes',
    );

    // Inbound A2A server half (smallhead -> DSH): a second prefix serves the
    // symmetric /a2a/messages + /a2a/tasks/:id endpoints, so the smallhead
    // `send` client can target `http://127.0.0.1:<port>/smallhead`.
    const subagents = ctx.get('subagents');
    const agents = ctx.get('agents');
    const a2aTasks = new Map();
    ctx.effect(
      () =>
        webServer.register({
          kind: 'prefix',
          path: '/smallhead/a2a',
          handler: async (req, res) => {
            try {
              const url = new URL(req.url ?? '/', 'http://127.0.0.1');
              const rest = url.pathname.slice('/smallhead/a2a'.length);

              if (req.method === 'POST' && rest === '/messages') {
                const chunks = [];
                for await (const chunk of req) chunks.push(chunk);
                let message = {};
                try {
                  message = Buffer.concat(chunks).toString('utf8') === '' ? {} : JSON.parse(Buffer.concat(chunks).toString('utf8'));
                } catch {
                  sendJson(res, 400, { error: 'invalid-json' });
                  return;
                }
                const reason = validateInboundA2a(message);
                if (reason !== '') {
                  sendJson(res, 400, { error: reason });
                  return;
                }
                const taskId = `task_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`;
                const queued = {
                  task_id: taskId,
                  message_id: message.message_id || '',
                  conversation_id: message.conversation_id || '',
                  status: 'queued',
                  from: 'dsh',
                  to: message.from || '',
                  text: '',
                  errors: [],
                };
                a2aTasks.set(taskId, queued);
                const base = {
                  task_id: taskId,
                  message_id: message.message_id || '',
                  conversation_id: message.conversation_id || '',
                  from: 'dsh',
                  to: message.from || '',
                };
                // The turn runs in the background and settles the task store on its
                // own; the request waits only a bounded time, so a slow turn answers
                // 202 queued instead of holding the socket open.
                const running = runDshTurn(subagents, agents, message.text)
                  .then((outcome) => {
                    a2aTasks.set(taskId, { ...base, status: outcome.status, text: outcome.text, errors: outcome.errors });
                  })
                  .catch((error) => {
                    const reason = String(error && error.message ? error.message : error);
                    a2aTasks.set(taskId, { ...base, status: 'error', text: reason, errors: [reason] });
                  });
                let inlineTimer;
                const settled = await Promise.race([
                  running.then(() => true),
                  new Promise((resolve) => {
                    inlineTimer = setTimeout(() => resolve(false), A2A_INLINE_WAIT_MS);
                  }),
                ]);
                clearTimeout(inlineTimer);
                sendJson(res, settled ? 200 : 202, a2aTasks.get(taskId));
                return;
              }

              if (req.method === 'GET' && rest.startsWith('/tasks/')) {
                const taskId = rest.slice('/tasks/'.length);
                const reply = a2aTasks.get(taskId);
                if (!reply) {
                  sendJson(res, 404, { error: `unknown A2A task: ${taskId}` });
                  return;
                }
                sendJson(res, 200, reply);
                return;
              }

              sendJson(res, 404, { error: 'not-found', prefix: '/smallhead/a2a' });
            } catch (error) {
              sendJson(res, 500, { error: String(error && error.message ? error.message : error) });
            }
          },
        }),
      'smallhead: inbound A2A routes',
    );
  }

  const tools = ctx.get('tools');
  if (tools) {
    ctx.effect(
      () =>
        tools.register({
          name: toolName,
          description:
            "Read the local smallhead agent workspace (read-only): recent sessions from the smallhead session index, or one session's metadata and trailing message previews. Use it to report what smallhead has been doing before answering questions about that project.",
          parameters: {
            type: 'object',
            properties: {
              sessionId: {
                type: 'string',
                description: 'One session id to inspect. Omit it to list recent sessions instead.',
              },
              limit: {
                type: 'integer',
                description: 'Maximum sessions to list (default 20).',
              },
              previewMessages: {
                type: 'integer',
                description: 'Trailing messages to preview for one session (default 8, max 60).',
              },
            },
          },
          output: {
            schema: {
              type: 'object',
              additionalProperties: true,
              properties: {
                sessionsDir: { type: 'string' },
                total: { type: 'integer' },
                sessions: {
                  type: 'array',
                  items: {
                    type: 'object',
                    additionalProperties: true,
                    properties: {
                      id: { type: 'string' },
                      createdAt: { type: 'integer' },
                      updatedAt: { type: 'integer' },
                      messageCount: { type: 'integer' },
                      preview: { type: 'string' },
                    },
                  },
                },
                session: {
                  type: 'object',
                  additionalProperties: true,
                  properties: {
                    id: { type: 'string' },
                    createdAt: { type: 'integer' },
                    updatedAt: { type: 'integer' },
                    messageCount: { type: 'integer' },
                    systemPromptLength: { type: 'integer' },
                    messages: {
                      type: 'array',
                      items: {
                        type: 'object',
                        additionalProperties: true,
                        properties: {
                          role: { type: 'string' },
                          text: { type: 'string' },
                        },
                      },
                    },
                  },
                },
                unavailable: {
                  oneOf: [
                    { type: 'null' },
                    {
                      type: 'object',
                      properties: { reason: { type: 'string' } },
                      required: ['reason'],
                      additionalProperties: false,
                    },
                  ],
                },
              },
            },
            render(_args, value) {
              return [{ type: 'text', text: JSON.stringify(value, null, 2) }];
            },
          },
          async execute(args) {
            const input = args && typeof args === 'object' ? args : {};
            if (typeof input.sessionId === 'string' && input.sessionId !== '') {
              return await sessionDetail(input.sessionId, input.previewMessages);
            }
            return await listSessions(input.limit);
          },
        }),
      'smallhead: observer tool',
    );

    ctx.effect(
      () =>
        tools.register({
          name: 'smallhead_a2a_send',
          description:
            "Send a task to the local smallhead A2A bridge. Defaults to read-only research. Set development=true to request file changes, shell commands, tests and service operations from a host started with --access development. The host enforces capabilities. Pass conversation_id from the previous reply to continue with context. queued/waiting is not completion; inspect status and errors before claiming success.",
          parameters: {
            type: 'object',
            properties: {
              text: { type: 'string', description: 'Message text to send to smallhead.' },
              peer: { type: 'string', description: 'Optional A2A bridge base URL (default http://127.0.0.1:3999).' },
              intent: { type: 'string', description: 'Optional intent label (default research).' },
              development: { type: 'boolean', description: 'Request file writes and command execution; requires a development-mode smallhead host. Default false.' },
              conversation_id: { type: 'string', description: 'Reuse the conversationId returned by an earlier call.' },
            },
            required: ['text'],
          },
          output: {
            schema: {
              type: 'object',
              additionalProperties: true,
              properties: {
                ok: { type: 'boolean' },
                status: { type: 'string' },
                text: { type: 'string' },
                taskId: { type: 'string' },
                conversationId: { type: 'string' },
                errors: { type: 'array', items: { type: 'string' } },
              },
            },
            render(_args, value) {
              return [{ type: 'text', text: JSON.stringify(value, null, 2) }];
            },
          },
          async execute(args) {
            const input = args && typeof args === 'object' ? args : {};
            const text = typeof input.text === 'string' ? input.text : '';
            if (text.trim() === '') return { ok: false, status: 'error', text: 'text is required', taskId: '', errors: ['text is required'] };
            return await a2aSend(input.peer || a2aPeer, text, typeof input.intent === 'string' ? input.intent : 'research', 120_000, input);
          },
        }),
      'smallhead: a2a send tool',
    );
  }
}
