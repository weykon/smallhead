# dsh-smallhead — DSH ↔ smallhead 观察插件

> 一个挂在 DeepSeek Harness（下称 DSH）上的持久化插件：把本机 smallhead 的会话数据读进 DSH 的 Web GUI，并给 Agent 一个 `smallhead_status` 工具。
>
> 本文档既讲**怎么用**，也讲**它怎么运作**——两半代码、数据怎么走、边界在哪、改动为什么需要重启。想改这个插件的人应该先读完本文。

## 1. 这个插件做什么

| 能力 | 入口 | 说明 |
|---|---|---|
| 会话浏览面板 | 左侧栏「smallhead」图标 → 中央 `main` 面板 | 列出 `~/.smallhead/sessions/index.json` 的最近会话，展开可看某条会话尾部消息 |
| Agent 工具 | `smallhead_status`（模型可见） | 列会话 / 查单条会话详情，只读 |
| 工具卡片 | 会话流里 `smallhead_status` 的调用结果 | 把工具返回的 JSON 渲染成可点击的会话 chip，点击跳到面板并高亮该会话 |
| A2A 发送 | `smallhead_a2a_send` 工具 + 面板 composer | POST 到 smallhead 的 A2A bridge 并轮询任务结果（`POST /smallhead/api/a2a/send`） |
| A2A 接收 | `POST /smallhead/a2a/messages` + `GET /smallhead/a2a/tasks/:id` | 收 smallhead 发来的消息，经 `ctx.subagents.start` 跑一个 DSH turn 回填 |

**边界**：会话浏览只读；A2A 发送默认只读，在 `smallhead_a2a_send` 填 `development:true` 可请求开发能力，smallhead 服务端需以 `--access development --cwd 项目路径` 启动。返回的 `conversationId` 可作为下一次调用的 `conversation_id`。接收进入 DSH 的路由仍拒绝 `mutate/execute/forward`，不因发送端能力扩展而改变。A2A 协议细节见 [docs/a2a-protocol.md](../../docs/a2a-protocol.md)。修改插件后需重新加载 DSH 插件实例，已缓存的实例不会自动读取新代码。

## 2. 运行拓扑：两个进程、两个半边

```
┌────────────────────────────── DSH Host（Node 进程）──────────────────────────────┐
│  Cordis Loader                                                                    │
│    └─ row `smallhead` → import `@local/dsh-smallhead` → index.js                 │
│         apply(ctx) 注册三样：                                                     │
│           ① webServer.register(prefix '/smallhead/api')   ← HTTP 只读路由        │
│           ② tools.register({ name: 'smallhead_status' })  ← Agent 工具           │
│           ③ （clientModules 扫描 dsh.client）→ 把 client.js 编入 boot graph      │
│                                                                                   │
│  dsh-host-webserver  监听 127.0.0.1:3080                                         │
│    /plugins/<pkg>/client.js?rev=…  ← 把 client.js 字节发给浏览器                 │
│    /smallhead/api/sessions         ← 本插件注册的路由                            │
│    /smallhead/api/session?id=…     ← 本插件注册的路由                            │
└───────────────┬───────────────────────────────────────────────┬──────────────────┘
                │ ① HTML + __DSH_BOOT__（含 boot graph）         │ ② 浏览器 fetch()
                ▼                                               │
┌─────────────────────────────── 浏览器（Web GUI）────────────────┴──────────────────┐
│  __ModuleLoader__ 惰性加载 client.js → 注册 factory（此时还没跑业务代码）          │
│  插件 apply(ctx)：                                                                │
│     ctx.locale.register('smallhead', {zh, en})                                    │
│     ctx.effect(insertStyles)          → <style> 注入                             │
│     ctx.slots.inject('main')              → main:smallhead        （中央面板）    │
│     ctx.slots.inject('tool.call.toolview')→ key:smallhead_status （会话流卡片）   │
│     ctx.slots.inject('sidebar.panellist') → id:smallhead          （侧栏图标）    │
└───────────────────────────────────────────────────────────────────────────────────┘
                │
                │ ③ smallhead_status 工具调用（模型 → Host 工具注册表）
                ▼
┌──────────────────────────── smallhead 本机存储（文件系统）────────────────────────┐
│  ~/.smallhead/sessions/index.json        ← JsonFileStore 每次 save 后重写         │
│  ~/.smallhead/sessions/<uuid-v7>.json    ← 全量快照（原子写：tmp → rename）       │
└───────────────────────────────────────────────────────────────────────────────────┘
```

关键点：**两半之间没有 RPC**。浏览器半边不调用 Host 半边的函数，它只 fetch 同一个 HTTP 服务器上的路由。图上 ①②③ 就是全部的数据通路。

## 3. 启动时序（谁在什么时候做什么）

1. **Host 启动**：Loader 读 profile（`package.json` 的 `dsh.profile.bundles` + `cordis.patch.yml`），我们的 row 被插入 → `import('@local/dsh-smallhead')` → 拿 `apply` 与 `inject` 两个导出。
2. **依赖门**：row 声明 `inject = ['webServer', 'tools']`。这两个服务没就绪，`apply` 不会被调用——插件保持 inactive，不报错。这就是为什么"插件没活"最常见的原因是依赖服务缺席，而不是代码抛错。
3. **`apply(ctx, config)`**：注册 HTTP 路由（`ctx.effect` 包住 disposer）与工具（同样包住）。
4. **clientModules 扫描**：Loader 里每个启用行如果 `package.json` 声明了 `dsh.client`，其 `./client` 导出被收进 **boot graph**（含 `dsh.client.platform / immediately / inject / external`）。
5. **浏览器启动**：index.html 注入 `window.__DSH_BOOT__`（boot graph）→ `__ModuleLoader__.load({id, factory})` 注册 factory。**此时业务代码还没跑**——factory 只有被 materialize 才执行。
6. **插件激活**：factory 执行 → 我们的 `apply(ctx)` 跑 → locale 注册、样式注入、三个 slot 注册。面板从这一刻起每 8 秒轮询一次路由。
7. **首次数据到达**：`fetch('/smallhead/api/sessions?limit=50')` → Host handler 读 `index.json` → 排序取前 N → JSON 回浏览器。

## 4. 三个注册物（这是插件的全部外向面）

| 注册 | 位置 | 谁调用它 | 拿到什么 |
|---|---|---|---|
| `webServer.register({kind:'prefix', path:'/smallhead/api'})` | Host | 任何同源 HTTP 客户端（浏览器 fetch、curl） | `{sessionsDir, total, sessions[], unavailable}` 或 `{session:{…}}` |
| `tools.register({name:'smallhead_status'})` | Host | **只有 Agent 循环**（模型决定调用） | 同上两个形状之一，经 `output.render` 变成一段文本 |
| `ctx.slots.register('main' / 'tool.call.toolview' / 'sidebar.panellist')` | Client | 宿主 UI 在对应位置渲染 | 各 slot 的 props（见下） |

同一个读逻辑（`listSessions` / `sessionDetail` 两个函数）同时服务路由和工具——**一次操作，两个调用方**。这是刻意的：面板和 Agent 不会各自漂移。

## 5. Host 半边实现（`index.js`）

```js
export const inject = ['webServer', 'tools'];

export function apply(ctx, config = {}) {
  const sessionsDir = resolveSessionsDir(config.sessionsDir);   // 默认 ~/.smallhead/sessions

  async function listSessions(limit)   { /* 读 index.json → 归一化 → 排序 → 截断 */ }
  async function sessionDetail(id, n)  { /* 校验 id → 读 <id>.json → 取尾部 n 条预览 */ }

  const webServer = ctx.get('webServer');
  if (webServer) ctx.effect(() => webServer.register({ kind: 'prefix', path: '/smallhead/api', handler }), '…');

  const tools = ctx.get('tools');
  if (tools) ctx.effect(() => tools.register({ name: 'smallhead_status', … }), '…');
}
```

设计要点：

- **读路径防御式**：`index.json` 不存在 → 空列表（不是错误，全新 checkout 就是这样）；JSON 坏掉 → `unavailable.reason`，面板显示 warning 而不是炸掉。
- **id 白名单**：`/^[0-9a-fA-F-]{8,64}$/`。任何 id 都要先过这一关，因为 id 会被拼进文件路径。`?id=../../etc/passwd` 返回 `sessionId must be a uuid-like string`。
- **裁剪**：单条消息预览截 400 字符、默认取尾部 8 条（上限 60）。面板不需要整份 transcript，也不应该把整份 transcript 塞进浏览器。
- **`limit` 收敛**：`clampLimit(value, default, max)`，调用方给什么都不会超过 `maxLimit`（默认 200）。
- **零 harness import**：见 §9 的硬约束。

**工具定义注意**：`tools.register` 接受的是普通对象（`ToolDefinition`），不是 `defineTool()` 的产物。`parameters` 是**裸 JSON Schema**（`type:'object'` + `properties`），`output` 是 `{ schema, render }`。这样插件不需要从 bundle 目录解析 `@deepseek-ai/dsh-tools`。

## 6. Client 半边实现（`client.js`）

文件形状（普通 DSH client bundle）：

```js
window.__ModuleLoader__.load({
  id: '@local/dsh-smallhead',
  factory(require) {
    const React = require('react');          // 平台种子表提供，不需要 npm 依赖
    /* …组件… */
    return { inject: ['slots', 'locale'], apply(ctx) { /* 注册 */ } };
  },
});
```

三个注册点各自的 props 与用途：

| slot | 形态 | 我们收到 | 我们返回 |
|---|---|---|---|
| `main` | keyed，`key: 'smallhead'` | `{ t }`（我们的 inject face）+ 标准 props | 面板组件 |
| `sidebar.panellist` | list，`id: 'smallhead'`, `order: 20` | `{ size, active }` | 图标组件 |
| `tool.call.toolview` | keyed，`key: 'smallhead_status'` | `{ phase, block, useDisclosure, toolName, … }` + `{ t, layout }` | 卡片组件 |

**工具卡的 `block` 形状**（关键）：

```js
phase: 'preparing'                      → 还没派发参数
phase: 'start'                          → block.argsRaw（原始参数 JSON 字符串）
phase: 'result'                         → block.content: ContentBlock[]，block.isError: boolean
                                          block.call: { name, argsRaw } | null
```

工具卡解析数据靠 `block.content`：我们 `output.render` 只产出一个 `{type:'text', text: JSON}` 块，所以 `JSON.parse(content[0].text)` 就是 payload。

**跨视图联动**（面板 ↔ 卡片）用 module-scope store，不用 Cordis service：

```js
let focusedSession = '';
const focusListeners = new Set();
function focusSession(id) { focusedSession = id; for (const l of [...focusListeners]) l(); }
```

为什么不用 `ctx.provide`：整份 client bundle 只有一个 module 实例，卡片和面板本来就在同一个闭包里，加一层 service 注册只是仪式。**代价**：这个 store 不跨浏览器标签页、不持久化、插件卸载后仍在内存里（无害，因为 listener 会被组件卸载时清掉）。

## 7. 端到端流程：一次点击

```
用户点会话 chip（在会话流的工具卡里）
  → onClick: focusSession(id)            // module-scope store，通知已挂载的面板
  → layout.selectPanel('smallhead')      // client `layout` service：切中央面板
  → 面板的 focus listener 触发 setFocused(id)
  → 面板下一次渲染把该会话排到第一位 + 加 .sh-row-focused（brand 色边框）
```

```
用户点面板里的「展开」
  → 组件内 useState(open) 翻转
  → useEffect 里 fetchSession(id) → GET /smallhead/api/session?id=…
  → Host handler：sessionDetail() → 读 <id>.json → 尾部 8 条
  → 组件渲染 角色 | 文本 列表
```

```
用户问 Agent「smallhead 最近跑咗咩」
  → 模型决定调用 smallhead_status（工具描述 + 参数 schema 由 Host 注册表提供）
  → 参数经校验 → execute(args) → listSessions(limit)
  → output.render 把返回值序列化成一段文本 → 写入会话流（durable event）
  → Client 的 tool.call.toolview（key 命中 'smallhead_status'）渲染卡片
```

注意最后一步：**会话流里存的是工具返回值本身，卡片只是它的一个视图**。所以卡片不参与数据获取，它不 fetch——这一点让卡片可重放（refresh / 换设备 / 回放历史都能渲染）。

## 8. 生命周期与卸载

- `ctx.effect(fn, label)`：`fn` 返回 disposer。插件被禁用/卸载时，**所有** effect 的 disposer 按注册顺序逆序执行 → 路由摘除、工具反注册、`<style>` 移除、slot 注册撤销、侧栏图标消失。
- Client 组件的资源（定时器、AbortController、store listener）都在 `useEffect` 的 cleanup 里释放。轮询用 `setInterval` + cleanup 的 `clearInterval`，fetch 用 `fetchJson` 的 8 秒 abort。
- Host 与 Client 的生命周期**不同步**：Host row 失败/禁用，Client 半边不会 compose；但 Client 半边出问题（例如组件抛错）不会影响 Host 工具。

## 9. 已踩过的硬约束（改这个插件前必读）

1. **link 出 profile 的 bundle 不能 import `@deepseek-ai/*`。**
   Bundle 真实路径是 `/Users/weykon/Desktop/p/small-head/plugins/dsh-smallhead`，而 harness 包在 npx 安装目录里。Node 的 ESM 解析按**真实路径**向上找 `node_modules`，所以 `import z from '@deepseek-ai/schemastery'` 直接 `ERR_MODULE_NOT_FOUND`（即使 profile 的 `node_modules` 里有）。
   → 所以：Host 半边零 harness import，工具用普通对象定义；也不再导出 `Config` schema。row config 改成运行时防御式读取（未知字段被忽略，打错字静默用默认值）。

2. **Host row 的 import 失败会被 Loader 缓存，重启才会清。**
   第一次安装时 import 失败（约束 1），修好代码后：`Config.listConfigs` 仍报 inactive、`install_bundle` 回 `ambiguous-install`（pnpm 无变更）、`set_plugin` 也清不掉——只有重启宿主才重新 import。

3. **Client bundle 改动同样要重启宿主。**
   `clientModules` 在启动时读 `client.js` 字节，`artifactRevision = hash(mtimeMs, ctimeMs, size)`，而重新读取**只**通过 HMR watcher 的 `rebuilt()` 回调。HMR 默认 `root: ['.']`、`ignored: ['**/node_modules', …]`——从 profile 目录 link 出去的包**不在 watch 范围**。所以：刷新浏览器不够（boot payload 里 `?rev=` 不变），必须重启。

4. **`ctx.slots.register` 的 `key` 打错就永远不渲染。**
   `tool.call.toolview` 是 keyed slot，key 必须**逐字**等于 wire 工具名。未命中的 key 落到 generic 行。

5. **Hook 不能放在 early return 之后。**
   工具卡在 `phase !== 'result'` 时要返回 `null`，但 `useDisclosure`/`useState` 必须在返回之前调用，否则 React 报 "Rendered more hooks"，slot entry 直接 crash 白屏。

6. **`insertStyles` 自己拥有 DOM `<style>`。**
   `styles.insert()` 属于动态 Client-half API；本文件是**普通 package client entry**（经 `__ModuleLoader__` 加载），所以自己 `document.createElement('style')` + `document.head.append`，并在 disposer 里 `remove()`。

## 10. 配置与调试

**配置字段**（写进 profile 的 `~/.dsh/profiles/web/cordis.patch.yml`；`config` 是整体替换，要重列所需字段）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `sessionsDir` | `~/.smallhead/sessions` | 会话存储目录 |
| `statusToolName` | `smallhead_status` | 工具名（改了这个名字，client 的 `tool.call.toolview` key 也要跟着改） |
| `defaultLimit` | 20 | 未传 `limit` 时列出多少条 |
| `maxLimit` | 200 | `limit` 硬上限 |

**验证命令**：

```bash
# 路由直连（不经 GUI）
curl -s "http://127.0.0.1:3080/smallhead/api/sessions?limit=5" | python3 -m json.tool
curl -s "http://127.0.0.1:3080/smallhead/api/session?id=<uuid>&preview=10"

# 免审批的运行时检查（Host inactive / 工具是否注册 / slot 是否有 occupant）
#   Config.listConfigs   name = @local/dsh-smallhead
#   Tool.listTools       → 是否含 smallhead_status
#   Slots.listSubTree    root = sidebar.panellist | main | tool.call.toolview
```

**常见症状对照**：

| 症状 | 原因 | 处置 |
|---|---|---|
| 侧栏无图标、工具也不在 | Host row 没活（依赖缺失 / import 失败 / 被禁用） | 查 `Tool.listTools`；改了 host 代码就重启 |
| 工具在、图标不在 | Client bundle 是旧缓存 | 重启宿主 |
| 面板 `Read failed: HTTP …` | 路由没注册（webServer 缺失）或 handler 抛错 | `curl` 直连路由看响应 |
| `Partially unavailable: index.json unreadable` | `sessionsDir` 不对或 JSON 坏 | 核对路径；`config.sessionsDir` 覆盖 |
| 列表空但 `total > 0` | 客户端 `limit` / 排序异常 | 对比路由返回与面板渲染 |
| `slot entry crashed in 'main'` | 组件抛错（最常见：hook 顺序） | 看浏览器 console 里 plugin 标签日志 |

## 11. 后续切片

| 切片 | 内容 | 状态 |
|---|---|---|
| P2 实时化 | `fs.watch` + SSE（`registerUpgrade`）取代 8 秒轮询 | 未做 |
| P3 事件流 | smallhead 加 `--emit-events <path>`，把 `DiagnosticEvent` 写 JSONL；插件跟随后出「本轮时间线」 | 未做 |
| S3 A2A（本分支已做） | smallhead-server 起 A2A bridge（server + client）；DSH 插件加 `smallhead_a2a_send` 工具 + `/smallhead/a2a/*` 接收端点 | ✅ `feat/a2a-dsh-bridge` |
| S4 A2A 到其他 peer | DSH↔DSH 用 `connection` + `webhookRuntime`；smallhead↔big-head 已用 `smallhead-server send http://127.0.0.1:3040` 验证 | 部分（跨 peer 已验，DSH↔DSH 未做） |

## 12. 文件清单

| 文件 | 作用 |
|---|---|
| `package.json` | bundle manifest：`dsh.bundle.patch` + `dsh.client`（`platform: 'web'`, `immediately: true`）+ icon/locale 导出 |
| `cordis.patch.yml` | 插入 Loader row：`id: smallhead`, `name: @local/dsh-smallhead` |
| `index.js` | Host 半边：路由 + 工具 + 读逻辑 |
| `client.js` | Client 半边：面板 + 侧栏图标 + 工具卡 + module-scope focus store |
| `icon.svg` | Plugin Manager 卡片图标 |
| `locale/{zh,en}.json` | Plugin Manager 卡片的标题与描述 |

## 13. 术语对照

| 术语 | 含义 |
|---|---|
| bundle | 一个有 `dsh.bundle.patch` 的 npm 包；安装后把若干 row 插入 Loader |
| row | Loader 表里的一项：`id` / `name`（包名）/ `config` / `disabled` / `inject` / `isolate` |
| service | Cordis 的服务：`ctx.get('webServer')`、`ctx.tools`、`ctx.slots`… row 用 `inject` 声明依赖 |
| slot | Client 的渲染座位；`main`/`sidebar.panellist` 是 list/keyed，`tool.call.toolview` 按工具名 keyed |
| boot graph | Host 启动时算出的 client bundle 图，注入成 `window.__DSH_BOOT__` |
| toolview | `tool.call.toolview` 的注册物：某个工具调用的自定义渲染 |
| effect | `ctx.effect(fn, label)`；`fn` 返回 disposer，随插件/scope 卸载自动执行 |
| HMR | 宿主端热替换：只覆盖模块与配置重载，**不**覆盖我们这种 link 出 profile 的 bundle |
