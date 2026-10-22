# smallhead A2A 协议

> smallhead 与 DSH（或任何本地 agent）之间的对等 Agent-to-Agent 互通协议。
> 参考 big-head 的 `src/a2a.rs`，但把协议做成**对称**的：smallhead 既能 *接收*
> 消息（`smallhead-server serve`），也能 *发送* 消息（`smallhead-server send`）。

## 1. 设计原则

1. **HTTP 只负责校验、排队、轮询**，真正的 agent turn 在独立任务里跑（`crate::driver::run_turn`），慢模型不会卡住请求线程。
2. **`from` 是自述（claim），不是身份**。本桥只监听 loopback，`from` 不能作为信任依据。默认 observe 模式；本地启动可选择 development 模式。
3. **协议对称**：两端的 agent-card / messages / tasks 端点形状一致，任何一端都可以是 server 或 client。
4. **入口与实际工具同时检查权限**：本地启动配置决定能力上限，请求 permissions 决定本轮工具子集。历史和 Lens 不能增加未注册工具。

## 2. 端点

| 方法 | 路径 | 用途 |
|---|---|---|
| GET | `/.well-known/agent-card.json` | 能力卡：名字、协议版本、能力、限额 |
| GET | `/health` | 存活探测，返回 `ok` |
| POST | `/a2a/messages` | 提交一条消息，创建任务并运行一个 turn |
| GET | `/a2a/tasks/:task_id` | 轮询任务结果 |

默认监听 `127.0.0.1:3999`（可用 `--port` 覆盖；避开 big-head 的 3040）。

## 3. 消息形状（`A2aMessage`）

```json
{
  "message_id": "msg_<uuid>",        // 可省略，服务端自动生成
  "conversation_id": "conv_<uuid>",  // 可省略；相同 id 复用会话上下文
  "from": "dsh",                     // 发送方自述名，非身份
  "to": "smallhead",                 // 目标；只能等于本 agent 名或 "*"
  "kind": "request",                 // 非空，≤128 字节
  "intent": "research",              // 可选
  "text": "……",                     // 1..16384 字节
  "reply_to": null,                  // 可选
  "permissions": {                   // 见 §4
    "can_observe": true,
    "can_plan": true,
    "can_mutate": false,
    "can_execute": false,
    "can_forward": false
  },
  "refs": [],                        // ≤32 个
  "ttl": 3                           // 1..8
}
```

## 4. 权限

| 字段 | observe 模式 | development 模式 |
|---|---|---|
| `can_observe` | 允许 | 允许 |
| `can_plan` | 允许 | 允许 |
| `can_mutate` | 拒绝 | 请求显式为 true 才注册 write/edit |
| `can_execute` | 拒绝 | 请求显式为 true 才注册 bash/process_control，且必须同时允许 mutate |
| `can_forward` | 拒绝 | 拒绝；尚未实现跨 peer 自动转发 |

超出本地能力上限，或 execute=true 但 mutate=false → `403`。任意 shell 能修改文件，因此不声称支持“只读 shell”。

只读工具包括 read、artifact_read、web_search、agent_reach、monad_bridge 和 monad_delegate；can_plan 控制 update_plan。研究/审查子 Monad 仅获得宿主选择的读取与搜索工具，不继承主 Monad 的写入、shell 或服务权限。A2A 暂无 ask_user_choice / spec 审阅面板，缺少必要信息时由最终回复向请求方说明。

开发模式复用现有 AgentLoop、上下文压缩、Lens 和 ProcessManager/ServiceManager。各请求有独立任务 owner，回合结束清理任务后台命令，常驻服务跨请求复用；宿主保持一个健康调度器。CLI 收到 Ctrl+C / SIGTERM 时停止接单、取消当前任务、保存结果并回收受管进程与服务。A2A 默认使用 `~/.smallhead/a2a/runs`，与 TUI 的目录锁和服务登记分开；两端目前不共享服务列表。

`--cwd` 是工具工作目录，不是操作系统沙箱；开发模式的命令具有启动进程用户的权限。

```sh
# 先在环境中导出 MINIMAX_API_KEY；服务端不会自动读取 .env，缺少 key 时使用固定 mock。
cargo run -p smallhead-server -- serve --access development --cwd /path/to/project \
  --diagnostics-dir /tmp/smallhead-a2a-diagnostics

# 另一个终端或 agent：请求开发权限，完成实际修改与测试
cargo run -p smallhead-server -- send http://127.0.0.1:3999 \
  '读一下这个项目，修复一个明确的小问题并运行相关测试' --development

# 后续请求：复用回复中的 conversation_id
cargo run -p smallhead-server -- send http://127.0.0.1:3999 \
  '再检查刚才的修改' --development --conversation conv_...
```

可用 `--process-root` 和 `--sessions-dir` 指定独立测试目录。`--diagnostics-dir` 为每个 task 写一份 JSONL，记录实际模型请求、工具摘要、Lens 次序与继续/结束决定；单任务内容上限 32 MiB，末行注明省略事件数。日志写入失败会记录错误，已经执行的任务不会因后续日志失败而被重放。

## 5. 任务回复形状（`A2aTaskReply`）

```json
{
  "task_id": "task_<uuid>",
  "message_id": "msg_<uuid>",
  "conversation_id": "conv_<uuid>",
  "status": "done | queued | waiting | failed | cancelled",
  "from": "smallhead",
  "to": "dsh",
  "text": "……",
  "errors": []
}
```

## 6. 提交时序

```
POST /a2a/messages
  → validate（from/kind/to/text/ttl/权限/refs）
  → 分配 message_id / conversation_id / task_id
  → 任务表插入 status=queued
  → spawn 独立任务跑 run_turn（无头 MainMonad + AgentLoop）
  → 最多等 120s：
      完成  → 200 + status=done + text
      超时  → 202 + status=queued（稍后 GET /a2a/tasks/{task_id} 轮询）
```

`conversation_id` 相同 → 顺序执行、复用上一轮完成后的历史，避免同时提交覆盖消息。等待上一轮期间也可返回 queued。对话和任务查询索引当前在内存中；快照用于持久化观察，不表示服务器重启后自动恢复该 conversation_id 的上下文。

模型或 Lens 失败时，真实工具回执、候选回答和明确失败状态保留在快照中，errors 返回实际原因；未通过收尾策略的候选不当作成功交付。

## 7. 客户端（发送方）

`smallhead-server send <peer> <text>` 用同一套 `A2aMessage` 把消息 POST 到 peer，
打印返回的 `A2aTaskReply`。库层 `smallhead_server::a2a::A2aClient` 提供
`card()` / `send()` / `task()` 三个方法，供 DSH 插件或未来 CLI 复用。

## 8. 双向（smallhead ↔ DSH）

- **DSH → smallhead**：DSH 插件是 client（`smallhead_a2a_send` 工具 + 面板 composer），
  POST 到 smallhead 的 `/a2a/messages`，轮询 `/a2a/tasks/:task_id`。
- **smallhead → DSH**：DSH 插件经 `webServer.register` 注册对称端点，但用命名空间前缀
  `/smallhead/a2a`（`POST /smallhead/a2a/messages` + `GET /smallhead/a2a/tasks/:id`），
  避免与宿主顶层路由撞车。smallhead 的 `A2aClient` 以
  `http://127.0.0.1:<dsh-port>/smallhead` 为 base_url，即可把消息 POST 到
  `/smallhead/a2a/messages`。DSH 收到后经 `ctx.subagents.start(...)` 跑一个
  subagent turn，把结果写回任务表，smallhead 轮询取回。

DSH 侧「跑一个 turn」的形态：`subagents.start(provider, { prompt, parent, signal })`
→ `SubagentRun.result`（`Promise<SubagentResult>`）→ 折叠 `output` 里的 text 块。
`parent` 解析顺序：`agents.currentInitiator()` → `agents.roots()[0]` →
`agents.list()[0]`；无活 agent 时返回明确错误而非静默失败。

## 9. 与 big-head 的差异

| 项 | big-head | smallhead |
|---|---|---|
| 世界 | ECS Spark + `A2aOrigin` 组件 | 无头 `MainMonad` + `AgentLoop` |
| 溯源 | `A2aOrigin`（claimed_from 等） | 会话 `conversation_id` + `from` 自述 |
| 工具 | `read_only_tools()` 白名单 | 本地模式上限与请求权限共同决定实际工具 |
| 会话 | 不入 ECS 业务世界 | 消息历史按 `conversation_id` 存内存 |
| 协议 | `big-head-a2a-v1` | `smallhead-a2a-v1` |

## 10. 冒烟测试

```bash
cargo build -p smallhead-server
./target/debug/smallhead-server serve --port 3999 &

curl http://127.0.0.1:3999/health                          # ok
curl http://127.0.0.1:3999/.well-known/agent-card.json     # 能力卡

curl -s -X POST http://127.0.0.1:3999/a2a/messages \
  -H 'content-type: application/json' \
  -d '{"from":"dsh","to":"smallhead","kind":"request","text":"检查能力","ttl":3}'

# 反向（smallhead → big-head，跨 peer）
./target/debug/smallhead-server send http://127.0.0.1:3040 "你好，我是 smallhead"
```
