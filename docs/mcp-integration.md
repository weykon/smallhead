# MCP 接入与服务管理

状态：首轮已实现并通过本地协议测试。范围是 smallhead 作为 MCP client 使用工具；不是把 smallhead 发布为 MCP server。

## 运行结构

`TUI / Agent → mcp 工具 → McpManager → 官方 rmcp SDK → MCP server`

协议处理放在独立的 `smallhead-mcp` crate。core 通过 `ExternalTool` 注册宿主提供的 `ToolImpl`，不依赖 MCP SDK，也不为每个外部工具添加 enum 变体。MCP server 名和工具名分别传入，避免不同 server 的同名工具互相覆盖。

模型先查询连接目录，再获取特定 server 的工具名称、说明和输入 schema，最后显式调用。不会把所有 MCP 工具的完整 schema 塞入每次模型请求。工具结果继续经过现有 artifact projection 和 rolling history compaction。

## 服务页与所有权

服务页统一展示常驻能力，但两类后端分别维护真实状态：

| 连接 | 生命周期所有者 | 就绪依据 | 断开操作 |
| --- | --- | --- | --- |
| 普通受管服务 | 现有 ServiceManager/ProcessManager | 进程存活，或配置的健康与身份验证 | 停止本应用拥有的进程 |
| MCP stdio | McpManager 内的 SDK transport | initialize 握手、能力协商和工具发现成功 | 关闭连接并清理本应用启动的子进程 |
| MCP Streamable HTTP | McpManager 内的 SDK client | 同上 | 关闭本客户端连接，不停止远端服务 |

stdio 的 stdout 是 JSON-RPC 协议通道，不能被普通进程输出收集器消费。SDK 与 ProcessManager 不能同时启动或持有同一个 stdio 进程。MCP 连接展示在服务页，并不等于伪造普通服务的 health_url，或把 HTTP 200 当成 MCP 可用。

连接配置保存到文件；活连接和 PID 不跨重启恢复。启动只读取配置，连接由用户或 Agent 显式发起；握手成功后才显示就绪。断线后保留可见失败状态，不自动重放上一次工具调用。

如果以后需要由普通 ServiceManager 启动一个提供 HTTP MCP 的本地服务，先正常登记进程服务，再用 MCP client 连接它的端点。进程所有权和协议连接仍各自只有一个维护者。

## 首轮能力

- 本地 stdio、远程 Streamable HTTP。
- 配置目录、显式连接/断开、工具目录查询和工具调用。
- 连接/请求超时；每个连接独立协调，避免一个慢 server 卡住其他连接。
- SDK 子进程 stderr 不直接打印到 TUI；HTTP 请求凭据不出现在列表或日志。
- 失败与取消时禁止自动重试可能有副作用的工具。缺失结果仍表示结果未知，需要核验。
- 退出 TUI 时清理本客户端拥有的连接和 stdio 进程。

首轮的应用层预算为：最多 64 个配置服务、每个服务最多 128 个工具、最多 8 页工具目录、工具说明与结果各最多 4 MiB、单次调用参数最多 256 KiB。同一服务同时只接受一个操作，忙时明确返回，不排一个不可见队列。结果超过上限仍可能意味着远端操作已经执行；不能把“结果无法接收”解释成“操作未发生”。这些是应用层检查，不是对任意恶意 server 的完整内存隔离。

首轮不包含 OAuth 登录流程、MCP resources/prompts 的完整宿主交互、sampling/elicitation 回调、远程服务部署或自动安装 server。需要这些能力时另补真实的宿主接口，不能在模型提示词里假装已经支持。

## TUI 入口

`Ctrl+S` 打开服务页，`m` 在普通服务和 MCP 之间切换；`/mcp` 可以直接进入 MCP。列表中用上下键选择，Enter/Tab 切换详情焦点，详情中用上下键或 PageUp/PageDown 滚动，Esc 返回列表。

操作键：`c` 连接、`x` 断开、`r` 重连、`t` 查询工具、`R` 重载配置。也可输入 `/mcp connect NAME`、`/mcp disconnect NAME`、`/mcp reconnect NAME`、`/mcp tools NAME`、`/mcp reload`。重连连接本身，不重复上次工具调用。

配置默认位于 `~/.smallhead/mcp.json`，可通过 `SMALLHEAD_MCP_CONFIG` 指定其他文件。修改配置后显式重载；读取配置本身不启动服务。Agent 也通过同一个管理器访问这些连接，界面和 Agent 看到的是同一份实际状态。

配置示例（替换命令和端点后使用，不会自动安装这些程序）：

```json
{
  "mcpServers": {
    "local-demo": {
      "command": "/absolute/path/to/mcp-server",
      "args": [],
      "cwd": "/absolute/path/to/project",
      "description": "本地工具服务",
      "call_timeout_ms": 60000
    },
    "remote-demo": {
      "url": "https://example.com/mcp",
      "http_headers": { "Authorization": "DEMO_MCP_AUTH" },
      "description": "远程工具服务",
      "disabled": true
    }
  }
}
```

每项只能选 `command` 或 `url`。stdio 可额外指定 `env` 字符串映射；HTTP 的 `http_headers` 值是环境变量名称，例如 `DEMO_MCP_AUTH` 的实际值可以是 `Bearer ...`，不是在配置里填入令牌。连接、列举工具、调用的默认超时分别为 30/30/60 秒，可通过 `connect_timeout_ms`、`list_timeout_ms`、`call_timeout_ms` 调整，单项最大 300 秒。`disabled` 默认 false。

例如配置好服务后，可以对 Agent 说：

> 看看已配置的 MCP 服务，连接其中的 demo，告诉我它提供哪些工具。先不要执行会修改数据的操作。

## Rust SDK 与兼容性

使用 Model Context Protocol 官方 `rmcp` 3.5.1，固定版本，避免手写 JSON-RPC 握手、请求 ID、取消和传输协议。该版本最低 Rust 1.88；仓库当前 toolchain 已高于这一要求，但 workspace 中仍写 1.85，因此不能宣称新增 MCP crate 兼容 1.85。MCP crate 单独声明其实际最低版本。

## world 与记忆的边界

MCP 提供外部能力，不作为用户记忆的唯一事实来源。某个 MCP memory server 可以作为可替换的检索后端，但用户输入、已确认结果、引用文件、方案版本和来源链接仍应由 smallhead 自己保存。记忆设计另见 `world-memory-current-and-options.md`，本轮不据此引入新的用户级 session/world 操作流程。

## 验证与后续范围

```sh
cargo test -p smallhead-mcp --features test-server
cargo test -p smallhead-core -p smallhead-tui
cargo clippy -p smallhead-core -p smallhead-mcp -p smallhead-tui --all-targets --features smallhead-mcp/test-server -- -D warnings
cargo build -p smallhead-tui -p smallhead-server
```

本轮 11 个 MCP 测试覆盖本地 SDK stdio 服务与 loopback HTTP 服务，包括握手、工具发现、结构化与错误结果、连接/调用超时、调用取消、独立服务并发、显式重连、配置重载、HTTP 过期连接不重放，以及 Unix 启动器进程组清理。core 162 个、TUI 123 个测试通过，另有 1 个忽略。实际启动 TUI 验证 `/mcp` 入口与退出清理。未安装或连接真实第三方 MCP 服务，OAuth 与跨平台进程树行为不在这些测试的证明范围内。

stdio 应配置前台运行的服务；主动脱离进程组，或启动器先正常退出、留下后台后代的程序，不能承诺清理完整进程树。Windows 当前仅清理直接子进程。详细生命周期与大小限制见 `crates/smallhead-mcp/README.md`。
