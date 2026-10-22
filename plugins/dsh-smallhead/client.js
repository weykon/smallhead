/**
 * Client half of `@local/dsh-smallhead`.
 *
 * Two views over one read-only data source:
 *
 * - a main panel listing the smallhead sessions the Host observer route reports,
 *   expanding one session's trailing messages;
 * - a `smallhead_status` Tool card in the conversation, which renders the tool's
 *   JSON as session rows and focuses the clicked session in that panel.
 *
 * No writes, no prompt submission — that is the next slice (see the panel's own
 * note). Data reaches the browser over the Host routes registered by `index.js`
 * (`GET /smallhead/api/sessions`, `GET /smallhead/api/session?id=`), so this half
 * needs no Host RPC and no Harness Client package: React, `ctx`, `styles` and
 * `console` are the whole surface.
 */
window.__ModuleLoader__.load({
  id: '@local/dsh-smallhead',
  factory(require) {
    const React = require('react');
    const h = React.createElement;

    /** Locale namespace this package registers. */
    const NS = 'smallhead';
    /** Main-panel key, sidebar entry id, and the Host route prefix. */
    const PANEL_ID = 'smallhead';
    /** Read-only JSON routes the Host half registers. */
    const API = '/smallhead/api';
    /** Panel self-refresh period in milliseconds. */
    const POLL_MS = 8000;

    const zh = {
      'panel.title': 'smallhead',
      'panel.subtitle': '只读观察',
      'panel.refresh': '刷新',
      'panel.dir': '会话目录',
      'panel.count': '共 {count} 个会话',
      'panel.updated': '更新于 {time}',
      'panel.empty': '目录里还没有会话。在 smallhead 里跑一轮对话后它会自动保存。',
      'panel.error': '读取失败：{reason}',
      'panel.unavailable': '部分数据不可用：{reason}',
      'panel.loading': '读取中…',
      'panel.noSessions': '没有可显示的会话',
      'session.messages': '{count} 条消息',
      'session.open': '展开',
      'session.close': '收起',
      'session.empty': '这条会话没有消息',
      'session.more': '…还有 {count} 条较早消息未显示',
      'write.note': 'S1 只读：派发任务（写回 smallhead）属于下一片，尚未实现。',
      'card.title': 'smallhead 会话',
      'card.summary': '{count} 个会话 · 最近更新 {time}',
      'card.one': '会话 {id} · {count} 条消息',
      'card.openPanel': '在面板打开',
      'card.showList': '收起列表',
      'card.showListOpen': '展开列表',
      'card.showRaw': '原始输出',
      'card.showRawClose': '收起原始输出',
      'card.empty': '没有会话',
      'send.placeholder': '发送消息给 smallhead（A2A）…',
      'send.button': '发送',
      'send.sending': '发送中…',
      'send.reply': 'smallhead 回复：{text}',
      'send.error': '发送失败：{reason}',
    };

    const en = {
      'panel.title': 'smallhead',
      'panel.subtitle': 'read-only observer',
      'panel.refresh': 'Refresh',
      'panel.dir': 'Sessions',
      'panel.count': '{count} sessions',
      'panel.updated': 'updated {time}',
      'panel.empty': 'No sessions yet. One smallhead turn saves one, and it appears here.',
      'panel.error': 'Read failed: {reason}',
      'panel.unavailable': 'Partially unavailable: {reason}',
      'panel.loading': 'Loading…',
      'panel.noSessions': 'No sessions to show',
      'session.messages': '{count} messages',
      'session.open': 'Expand',
      'session.close': 'Collapse',
      'session.empty': 'This session has no messages',
      'session.more': '…{count} earlier messages not shown',
      'write.note': 'S1 is read-only: dispatching work back into smallhead is the next slice.',
      'card.title': 'smallhead sessions',
      'card.summary': '{count} sessions · latest {time}',
      'card.one': 'Session {id} · {count} messages',
      'card.openPanel': 'Open in panel',
      'card.showList': 'Hide list',
      'card.showListOpen': 'Show list',
      'card.showRaw': 'Raw output',
      'card.showRawClose': 'Hide raw output',
      'card.empty': 'No sessions',
      'send.placeholder': 'Send a message to smallhead (A2A)…',
      'send.button': 'Send',
      'send.sending': 'Sending…',
      'send.reply': 'smallhead replied: {text}',
      'send.error': 'Send failed: {reason}',
    };

    /**
     * Format epoch millis in the browser's own timezone.
     * @param {number} value - epoch millis, 0 when unknown.
     * @returns {string} display text.
     */
    function formatTime(value) {
      if (!Number.isFinite(value) || value <= 0) return '—';
      const date = new Date(value);
      if (Number.isNaN(date.getTime())) return '—';
      const pad = (n) => String(n).padStart(2, '0');
      return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
    }

    /**
     * Fill `{name}` placeholders in one localized string, never throwing on a
     * missing key so one dictionary gap cannot blank the slot.
     * @param {(key: string, params?: Record<string, unknown>) => string} t - bound translator.
     * @param {string} key - dictionary key.
     * @param {Record<string, unknown>} params - substitutions.
     * @returns {string} display text.
     */
    function fill(t, key, params) {
      try {
        return t(key, params);
      } catch {
        return key;
      }
    }

    /** Panel stylesheet, expressed through theme tokens so light and dark both work. */
    const CSS = `
.sh-panel{box-sizing:border-box;display:flex;flex-direction:column;gap:10px;padding:16px 18px 24px;min-height:100%;color:var(--dsw-alias-label-primary);overflow:auto}
.sh-head{display:flex;align-items:flex-end;gap:10px;flex-wrap:wrap}
.sh-title{font-size:16px;line-height:24px;font-weight:600;margin:0}
.sh-tag{border:.5px solid var(--dsw-alias-border-l1);border-radius:var(--dsw-radius-sm);padding:0 6px;font-size:11px;line-height:18px;color:var(--dsw-alias-label-tertiary)}
.sh-spacer{flex:1}
.sh-button{border:.5px solid var(--dsw-alias-border-l2);border-radius:var(--dsw-radius-sm);background:transparent;color:var(--dsw-alias-label-secondary);cursor:pointer;font-size:12px;line-height:18px;padding:3px 10px}
.sh-button:hover{background:var(--dsw-alias-interactive-bg-hover);color:var(--dsw-alias-label-primary)}
.sh-button:focus-visible{outline:var(--dsw-focus-ring-width) solid var(--dsw-focus-ring-color);outline-offset:1px}
.sh-meta{display:flex;gap:12px;flex-wrap:wrap;font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary)}
.sh-mono{font-family:var(--dsw-font-mono,monospace)}
.sh-notice{border-radius:var(--dsw-radius-sm);border:.5px solid var(--dsw-alias-border-l1);padding:6px 10px;font-size:12px;line-height:18px;color:var(--dsw-alias-label-secondary)}
.sh-notice-error{border-color:var(--dsw-alias-state-error-primary);color:var(--dsw-alias-state-error-primary)}
.sh-notice-warn{border-color:var(--dsw-alias-state-warn-primary)}
.sh-list{display:flex;flex-direction:column;gap:2px}
.sh-row{box-sizing:border-box;border-radius:var(--dsw-radius-sm);padding:8px 10px;display:flex;flex-direction:column;gap:4px;border:.5px solid transparent}
.sh-row:hover{border-color:var(--dsw-alias-border-l1)}
.sh-row-open{border-color:var(--dsw-alias-border-l2)}
.sh-row-focused{border-color:var(--dsw-alias-brand-primary);background:var(--dsw-alias-interactive-bg-hover)}
.sh-row-top{display:flex;align-items:center;gap:8px}
.sh-id{font-family:var(--dsw-font-mono,monospace);font-size:12px;line-height:18px;color:var(--dsw-alias-label-secondary)}
.sh-count{font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary);font-variant-numeric:tabular-nums}
.sh-time{font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary);font-variant-numeric:tabular-nums}
.sh-preview{font-size:12px;line-height:18px;color:var(--dsw-alias-label-primary);overflow-wrap:anywhere}
.sh-preview-empty{color:var(--dsw-alias-label-tertiary)}
.sh-detail{margin-top:6px;border-top:.5px solid var(--dsw-alias-border-l1);padding-top:8px;display:flex;flex-direction:column;gap:6px}
.sh-msg{display:flex;gap:8px;align-items:baseline}
.sh-role{flex:none;min-width:64px;font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary)}
.sh-text{font-size:12px;line-height:18px;overflow-wrap:anywhere;white-space:pre-wrap}
.sh-more{font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary)}
.sh-foot{font-size:11px;line-height:16px;color:var(--dsw-alias-label-tertiary)}
.sh-composer{display:flex;gap:8px;align-items:center;border-top:.5px solid var(--dsw-alias-border-l1);padding-top:10px}
.sh-composer-input{flex:1;box-sizing:border-box;border:.5px solid var(--dsw-alias-border-l2);border-radius:var(--dsw-radius-sm);background:transparent;color:var(--dsw-alias-label-primary);font:inherit;font-size:12px;line-height:18px;padding:5px 8px}
.sh-composer-input:focus{outline:var(--dsw-focus-ring-width) solid var(--dsw-focus-ring-color);outline-offset:1px}
.sh-composer-reply{font-size:12px;line-height:18px;color:var(--dsw-alias-label-secondary);overflow-wrap:anywhere;white-space:pre-wrap}
.sh-composer-reply-error{color:var(--dsw-alias-state-error-primary)}
.sh-card{box-sizing:border-box;border:.5px solid var(--dsw-alias-border-l1);border-radius:var(--dsw-radius-md);padding:8px 10px;display:flex;flex-direction:column;gap:6px;color:var(--dsw-alias-label-primary)}
.sh-card-head{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.sh-card-title{font-size:12px;line-height:18px;font-weight:600}
.sh-chips{display:flex;flex-direction:column;gap:2px}
.sh-chip{box-sizing:border-box;display:flex;align-items:center;gap:8px;width:100%;text-align:left;border:.5px solid transparent;border-radius:var(--dsw-radius-sm);background:transparent;color:inherit;cursor:pointer;padding:3px 6px;font:inherit}
.sh-chip:hover{border-color:var(--dsw-alias-border-l2);background:var(--dsw-alias-interactive-bg-hover)}
.sh-chip:focus-visible{outline:var(--dsw-focus-ring-width) solid var(--dsw-focus-ring-color);outline-offset:1px}
.sh-chip-preview{flex:1;min-width:0;font-size:12px;line-height:18px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--dsw-alias-label-secondary)}
.sh-raw{margin:0;padding:6px 8px;max-height:200px;overflow:auto;border-radius:var(--dsw-radius-sm);background:var(--dsw-alias-bg-layer-2);color:var(--dsw-alias-label-secondary);font-family:var(--dsw-font-mono,monospace);font-size:11px;line-height:16px;white-space:pre-wrap;overflow-wrap:anywhere}
`;

    /**
     * Attach this package's stylesheet for the lifetime of the Cordis effect.
     *
     * `styles.insert()` belongs to DSH's dynamically evaluated Client-half
     * API. This file is a regular package client entry loaded through
     * `__ModuleLoader__`, so it must own its DOM style element directly.
     * @param {string} css - raw stylesheet text.
     * @returns {() => void} disposer used when the plugin unloads.
     */
    function insertStyles(css) {
      const tag = document.createElement('style');
      tag.dataset.plugin = '@local/dsh-smallhead';
      tag.textContent = css;
      document.head.append(tag);
      return () => tag.remove();
    }

    /**
     * Sidebar icon: a small head, drawn with `currentColor` so it follows the rail.
     * @param {{ size?: number }} props - owner-supplied square edge.
     * @returns {import('react').ReactElement} svg icon.
     */
    function SmallheadIcon({ size }) {
      const edge = Number.isFinite(size) ? size : 16;
      return h(
        'svg',
        {
          viewBox: '0 0 24 24',
          width: edge,
          height: edge,
          fill: 'none',
          stroke: 'currentColor',
          strokeWidth: 1.6,
          strokeLinecap: 'round',
          'aria-hidden': true,
        },
        h('circle', { cx: 12, cy: 13, r: 6 }),
        h('path', { d: 'M12 7V4' }),
        h('circle', { cx: 12, cy: 3.4, r: 1.1, fill: 'currentColor', stroke: 'none' }),
        h('path', { d: 'M9.6 12.6h.01M14.4 12.6h.01' }),
        h('path', { d: 'M10.2 16c1.1.8 2.5.8 3.6 0' }),
      );
    }

    /**
     * Read one session's detail through the Host route.
     * @param {string} id - session id.
     * @param {AbortSignal} signal - abort signal.
     * @returns {Promise<object>} canonical detail value.
     */
    async function fetchSession(id, signal) {
      const response = await fetch(`${API}/session?id=${encodeURIComponent(id)}`, { signal });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return await response.json();
    }

    /**
     * One expandable session row, highlighted while it is the panel's focused session.
     * @param {{ session: object, t: Function, focused: boolean }} props - row props.
     * @returns {import('react').ReactElement} row.
     */
    function SessionRow({ session, t, focused }) {
      const [open, setOpen] = React.useState(false);
      const [detail, setDetail] = React.useState(null);
      const [failure, setFailure] = React.useState('');

      React.useEffect(() => {
        if (!open) return undefined;
        const controller = new AbortController();
        let live = true;
        fetchSession(session.id, controller.signal)
          .then((value) => {
            if (live) setDetail(value);
          })
          .catch((error) => {
            if (live && error && error.name !== 'AbortError') setFailure(String(error.message || error));
          });
        return () => {
          live = false;
          controller.abort();
        };
      }, [open, session.id]);

      const detailParts = [];
      if (open) {
        if (failure !== '') {
          detailParts.push(
            h('div', { key: 'error', className: 'sh-notice sh-notice-error' }, fill(t, 'panel.error', { reason: failure })),
          );
        } else if (detail === null) {
          detailParts.push(h('div', { key: 'loading', className: 'sh-more' }, fill(t, 'panel.loading', {})));
        } else if (detail.unavailable) {
          detailParts.push(
            h(
              'div',
              { key: 'unavailable', className: 'sh-notice sh-notice-warn' },
              fill(t, 'panel.unavailable', { reason: detail.unavailable.reason }),
            ),
          );
        } else if (detail.session) {
          const messages = Array.isArray(detail.session.messages) ? detail.session.messages : [];
          if (messages.length === 0) {
            detailParts.push(h('div', { key: 'empty', className: 'sh-more' }, fill(t, 'session.empty', {})));
          }
          messages.forEach((message, index) => {
            detailParts.push(
              h(
                'div',
                { key: `m${index}`, className: 'sh-msg' },
                h('span', { className: 'sh-role' }, message.role),
                h('span', { className: 'sh-text' }, message.text === '' ? '—' : message.text),
              ),
            );
          });
          const hidden = detail.session.messageCount - messages.length;
          if (hidden > 0) {
            detailParts.push(h('div', { key: 'more', className: 'sh-more' }, fill(t, 'session.more', { count: hidden })));
          }
        }
      }

      return h(
        'div',
        { className: `sh-row${open ? ' sh-row-open' : ''}${focused ? ' sh-row-focused' : ''}` },
        h(
          'div',
          { className: 'sh-row-top' },
          h('span', { className: 'sh-id', title: session.id }, session.id.slice(0, 8)),
          h('span', { className: 'sh-count' }, fill(t, 'session.messages', { count: session.messageCount })),
          h('span', { className: 'sh-time' }, formatTime(session.updatedAt)),
          h('span', { className: 'sh-spacer' }),
          h(
            'button',
            { type: 'button', className: 'sh-button', 'aria-expanded': open, onClick: () => setOpen(!open) },
            fill(t, open ? 'session.close' : 'session.open', {}),
          ),
        ),
        h(
          'div',
          { className: `sh-preview${session.preview === '' ? ' sh-preview-empty' : ''}` },
          session.preview === '' ? '—' : session.preview,
        ),
        detailParts.length > 0 ? h('div', { className: 'sh-detail' }, ...detailParts) : null,
      );
    }

    /**
     * The panel body: header, notices, and the session list.
     * @param {{ t: Function }} props - injected face.
     * @returns {import('react').ReactElement} panel.
     */
    function SmallheadPanel({ t }) {
      const [state, setState] = React.useState({ phase: 'loading', data: null, failure: '', at: 0 });
      const [focused, setFocused] = React.useState(focusedSession);
      const [sendText, setSendText] = React.useState('');
      const [sendState, setSendState] = React.useState({ phase: 'idle', text: '', error: '' });

      const send = React.useCallback(async () => {
        const text = sendText.trim();
        if (text === '') return;
        setSendState({ phase: 'sending', text: '', error: '' });
        const outcome = await fetchJson(`${API}/a2a/send`, POLL_MS * 4, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ text }),
        });
        if (outcome.ok && outcome.data && outcome.data.ok !== false) {
          setSendState({ phase: 'done', text: outcome.data.text ?? '', error: '' });
          setSendText('');
        } else {
          setSendState({ phase: 'error', text: '', error: (outcome.data && outcome.data.text) || outcome.reason || 'unknown error' });
        }
      }, [sendText]);

      const load = React.useCallback(async () => {
        const outcome = await fetchJson(`${API}/sessions?limit=50`, POLL_MS);
        if (outcome.ok) setState({ phase: 'ready', data: outcome.data, failure: '', at: Date.now() });
        else setState((previous) => ({ ...previous, phase: 'failed', failure: outcome.reason }));
      }, []);

      React.useEffect(() => {
        const onFocus = () => setFocused(focusedSession);
        focusListeners.add(onFocus);
        return () => {
          focusListeners.delete(onFocus);
        };
      }, []);

      React.useEffect(() => {
        load();
        const timer = setInterval(load, POLL_MS);
        return () => clearInterval(timer);
      }, [load]);

      const { phase, data, failure, at } = state;
      const sessions = data && Array.isArray(data.sessions) ? data.sessions : [];
      const unavailable = data && data.unavailable ? data.unavailable.reason : '';
      const dir = data && typeof data.sessionsDir === 'string' ? data.sessionsDir : '';
      // A focus request outranks recency until the next refresh, so a card click
      // lands on the session under the pointer instead of wherever it ranks.
      const ordered =
        focused === '' ? sessions : [...sessions].sort((a, b) => (a.id === focused ? -1 : b.id === focused ? 1 : 0));

      const head = h(
        'div',
        { className: 'sh-head' },
        h('h2', { className: 'sh-title' }, fill(t, 'panel.title', {})),
        h('span', { className: 'sh-tag' }, fill(t, 'panel.subtitle', {})),
        h('span', { className: 'sh-spacer' }),
        phase === 'ready' ? h('span', { className: 'sh-time' }, fill(t, 'panel.updated', { time: formatTime(at) })) : null,
        h('button', { type: 'button', className: 'sh-button', onClick: () => load() }, fill(t, 'panel.refresh', {})),
      );

      const meta = h(
        'div',
        { className: 'sh-meta' },
        dir === '' ? null : h('span', { className: 'sh-mono' }, `${fill(t, 'panel.dir', {})}: ${dir}`),
        data === null ? null : h('span', {}, fill(t, 'panel.count', { count: data.total })),
      );

      const notices = [];
      if (phase === 'failed') {
        notices.push(
          h('div', { key: 'failed', className: 'sh-notice sh-notice-error' }, fill(t, 'panel.error', { reason: failure })),
        );
      }
      if (unavailable !== '') {
        notices.push(
          h('div', { key: 'partial', className: 'sh-notice sh-notice-warn' }, fill(t, 'panel.unavailable', { reason: unavailable })),
        );
      }

      const list = [];
      if (phase === 'loading' && data === null) {
        list.push(h('div', { key: 'loading', className: 'sh-notice' }, fill(t, 'panel.loading', {})));
      } else if (sessions.length === 0) {
        list.push(h('div', { key: 'empty', className: 'sh-notice' }, fill(t, dir === '' ? 'panel.noSessions' : 'panel.empty', {})));
      } else {
        ordered.forEach((session) => {
          list.push(h(SessionRow, { key: session.id, session, t, focused: session.id === focused }));
        });
      }

      return h(
        'div',
        { className: 'sh-panel' },
        head,
        meta,
        ...notices,
        h('div', { className: 'sh-list' }, ...list),
        h('div', { className: 'sh-foot' }, fill(t, 'write.note', {})),
        h(
          'div',
          { className: 'sh-composer' },
          h('input', {
            className: 'sh-composer-input',
            value: sendText,
            placeholder: fill(t, 'send.placeholder', {}),
            onChange: (event) => setSendText(event.target.value),
            onKeyDown: (event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                send();
              }
            },
          }),
          h(
            'button',
            { type: 'button', className: 'sh-button', onClick: send, disabled: sendState.phase === 'sending' },
            fill(t, sendState.phase === 'sending' ? 'send.sending' : 'send.button', {}),
          ),
        ),
        sendState.phase === 'done' && sendState.text !== ''
          ? h('div', { className: 'sh-composer-reply' }, fill(t, 'send.reply', { text: sendState.text }))
          : null,
        sendState.phase === 'error'
          ? h('div', { className: 'sh-composer-reply sh-composer-reply-error' }, fill(t, 'send.error', { reason: sendState.error }))
          : null,
      );
    }

    /**
     * Fetch one JSON route, aborting after `timeoutMs`, and answer a discriminated
     * result so no caller has to catch. Every read path funnels through here.
     * @param {string} url - absolute route URL.
     * @param {number} timeoutMs - abort budget.
     * @returns {Promise<{ ok: true, data: object } | { ok: false, reason: string }>} outcome.
     */
    async function fetchJson(url, timeoutMs, options = {}) {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), timeoutMs);
      try {
        const response = await fetch(url, { signal: controller.signal, ...options });
        if (!response.ok) return { ok: false, reason: `HTTP ${response.status}` };
        return { ok: true, data: await response.json() };
      } catch (error) {
        return { ok: false, reason: String(error && error.message ? error.message : error) };
      } finally {
        clearTimeout(timer);
      }
    }

    /**
     * Session the panel is currently focused on. It lives at module scope because
     * the Tool card and the panel are two registrations of one bundle; the store
     * carries a click in the conversation to the panel without either one
     * reaching into the other's DOM or props.
     * @type {string}
     */
    let focusedSession = '';
    /** Focus listeners belonging to live panel instances. @type {Set<() => void>} */
    const focusListeners = new Set();

    /**
     * Focus one session for every mounted panel.
     * @param {string} sessionId - session id to highlight.
     */
    function focusSession(sessionId) {
      focusedSession = typeof sessionId === 'string' ? sessionId : '';
      for (const listener of [...focusListeners]) {
        try {
          listener();
        } catch (error) {
          console.error('smallhead: focus listener failed', error);
        }
      }
    }

    /**
     * Narrow the `smallhead_status` tool's rendered JSON text. The tool renders
     * one text block, so an unparsable payload is the only failure mode and it
     * leaves the card as a plain title row.
     * @param {readonly object[]} content - result content blocks.
     * @returns {object | null} the parsed value, or null when unusable.
     */
    function parseToolPayload(content) {
      if (!Array.isArray(content) || content.length !== 1) return null;
      const only = content[0];
      if (!only || only.type !== 'text' || typeof only.text !== 'string') return null;
      try {
        const parsed = JSON.parse(only.text);
        return parsed && typeof parsed === 'object' ? parsed : null;
      } catch {
        return null;
      }
    }

    /**
     * Render the `smallhead_status` tool call as session rows instead of raw JSON,
     * and take the panel to the clicked session.
     * @param {object} props - Tool owner currency plus this bundle's injected face.
     * @returns {import('react').ReactElement | null} the card.
     */
    function SmallheadStatusCard(props) {
      const { block, t, layout, useDisclosure } = props;
      // Hooks must run in every phase, so the disclosure hook is taken before any
      // early return; it falls back to local state if the owner omits it.
      const disclosure = typeof useDisclosure === 'function' ? useDisclosure() : React.useState(false);
      const [open, setOpen] = disclosure;
      const [showRaw, setShowRaw] = React.useState(false);

      if (!block || block.phase !== 'result') return null;
      const node = block.block;
      if (!node || Array.isArray(node.content) === false) return null;

      const payload = parseToolPayload(node.content);
      const sessions = payload && Array.isArray(payload.sessions) ? payload.sessions : [];
      const detail = payload && payload.session && typeof payload.session.id === 'string' ? payload.session : null;
      const rawText = node.content.map((part) => (part && part.type === 'text' ? part.text : '')).join('');

      const headText = detail
        ? fill(t, 'card.one', { id: detail.id.slice(0, 8), count: detail.messageCount })
        : sessions.length > 0
          ? fill(t, 'card.summary', { count: payload.total ?? sessions.length, time: formatTime(sessions[0].updatedAt) })
          : fill(t, 'card.empty', {});

      const body = [];
      if (detail) {
        body.push(
          h(
            'button',
            {
              key: `focus-${detail.id}`,
              type: 'button',
              className: 'sh-chip',
              title: detail.id,
              onClick: () => {
                focusSession(detail.id);
                if (layout && typeof layout.selectPanel === 'function') layout.selectPanel(PANEL_ID);
              },
            },
            h('span', { className: 'sh-id' }, detail.id.slice(0, 8)),
            h('span', { className: 'sh-count' }, fill(t, 'session.messages', { count: detail.messageCount })),
            h('span', { className: 'sh-spacer' }),
            h('span', { className: 'sh-more' }, fill(t, 'card.openPanel', {})),
          ),
        );
        if (typeof rawText === 'string' && rawText !== '') {
          body.push(
            h(
              'button',
              { key: 'raw-toggle', type: 'button', className: 'sh-button', onClick: () => setShowRaw(!showRaw) },
              fill(t, showRaw ? 'card.showRawClose' : 'card.showRaw', {}),
            ),
          );
          if (showRaw) body.push(h('pre', { key: 'raw', className: 'sh-raw' }, rawText));
        }
      } else if (sessions.length > 0) {
        body.push(
          h(
            'button',
            { key: 'list-toggle', type: 'button', className: 'sh-button', onClick: () => setOpen(!open) },
            fill(t, open ? 'card.showList' : 'card.showListOpen', {}),
          ),
        );
        if (open) {
          body.push(
            h(
              'div',
              { key: 'chips', className: 'sh-chips' },
              ...sessions.map((session) =>
                h(
                  'button',
                  {
                    key: session.id,
                    type: 'button',
                    className: 'sh-chip',
                    title: session.id,
                    onClick: () => {
                      focusSession(session.id);
                      if (layout && typeof layout.selectPanel === 'function') layout.selectPanel(PANEL_ID);
                    },
                  },
                  h('span', { className: 'sh-id' }, session.id.slice(0, 8)),
                  h('span', { className: 'sh-count' }, fill(t, 'session.messages', { count: session.messageCount })),
                  h('span', { className: 'sh-chip-preview' }, session.preview === '' ? '—' : session.preview),
                  h('span', { className: 'sh-time' }, formatTime(session.updatedAt)),
                ),
              ),
            ),
          );
        }
      }

      return h(
        'div',
        { className: 'sh-card' },
        h(
          'div',
          { className: 'sh-card-head' },
          h(SmallheadIcon, { size: 14 }),
          h('span', { className: 'sh-card-title' }, fill(t, 'card.title', {})),
          h('span', { className: 'sh-more' }, headText),
          h('span', { className: 'sh-spacer' }),
          h(
            'button',
            {
              type: 'button',
              className: 'sh-button',
              onClick: () => {
                if (layout && typeof layout.selectPanel === 'function') layout.selectPanel(PANEL_ID);
              },
            },
            fill(t, 'card.openPanel', {}),
          ),
        ),
        ...body,
      );
    }

    return {
      inject: ['slots', 'locale'],
      apply(ctx) {
        ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'smallhead: dictionaries');
        const t = ctx.locale.bind(NS);

        ctx.effect(() => insertStyles(CSS), 'smallhead: stylesheet');

        // Panel navigation is a soft dependency: without it the card and panel
        // still render, and only the cross-view jump is lost.
        const layout = ctx.get('layout');
        const face = { t, layout };

        // The main panel is keyed by the same id the sidebar button dispatches.
        ctx.slots.inject('main', () =>
          ctx.slots.register({ name: 'main', key: PANEL_ID, locale: NS, inject: () => ({ t }) }, SmallheadPanel),
        );

        // The `smallhead_status` tool call renders as session rows in the
        // conversation, so an answer about smallhead is itself navigable.
        ctx.slots.inject('tool.call.toolview', () =>
          ctx.slots.register({ name: 'tool.call.toolview', key: 'smallhead_status', locale: NS, inject: () => face }, SmallheadStatusCard),
        );

        // The sidebar row owns navigation: clicking it selects this panel.
        ctx.slots.inject('sidebar.panellist', () =>
          ctx.slots.register(
            { name: 'sidebar.panellist', id: PANEL_ID, order: 20, label: () => t('panel.title'), locale: NS },
            SmallheadIcon,
          ),
        );
      },
    };
  },
});
