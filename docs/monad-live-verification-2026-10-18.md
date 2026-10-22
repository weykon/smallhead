# Monad 真实运行验收：2026-10-18

## 结论

使用真实 MiniMax-M3，运行中追加要求、暂停子 Monad、主 Monad 转交另一子 Monad 的事实、恢复执行的链条通过验证。聚焦场景约 122 秒完成，两个子任务均 Completed，11 项离线日志核验通过。

但研究报告的事实准确性尚未过关：最终汇总包含错误的 MCP 生命周期推论。因此本次证明的是协作机制能够工作，不能据此认定多 Monad 自动研究的结论已经可靠。没有实现或验证 Lens 的动态组合。

## 实际运行入口

现有 HTTP A2A driver 仍注册旧 monad_bridge，不能覆盖这次的新委派控制。测试使用 TUI 的真实 Runtime、AgentLoop、Route::Auto 和 SteeringInbox，以无界面方式调用真实模型；没有使用 mock 代替模型，也没有验证鼠标、布局或终端渲染。

测试任务为只读源码研究，不连接 MCP、不启动或停止真实服务、不写业务文件。子 Monad 使用独立 AgentLoop，通过主 Monad 转交信息，未测试共享写入或递归委派。

## 三轮测试

| 轮次 | 结果 | 观察 |
| --- | --- | --- |
| A | 失败并终止 | 主 Monad 两次遗漏 attempt_id，把参数错误误判为能力缺失；Cargo 默认工作目录还造成源码发现困难。 |
| B | 失败并清理 | 追加要求、暂停、转交事实、恢复均发生；界面子任务最终触发旧的预算/截断/过滤混合错误。不能从旧错误确定唯一原因。 |
| C | 完成 | 提供准确文件路径、缩小研究范围；两个子任务完成，暂停期间没有启动新的子工具，事实进入接收方的真实 provider 上下文。 |

C 轮仍出现两个可恢复参数错误：计划同时设置两个 in_progress、wait 超过单次 30000ms 上限。模型收到错误后自行修正。

原始记录保留在本机临时目录，不属于仓库持久归档：

- `/tmp/smallhead-monad-live-20261018-a`
- `/tmp/smallhead-monad-live-20261018-b`
- `/tmp/smallhead-monad-live-20261018-c`

C 轮包含 events.jsonl、inputs.jsonl、runs.json、messages.json、answer.md、outcome.json、verification.json。events.jsonl 用于检查真实请求上下文；压缩后的 messages.json 不能替代完整事件记录。日志包含任务内容，分享前需要自行检查。

## C 轮可确认的执行顺序

| 相对时间 | 实际事件 |
| --- | --- |
| 约 8 秒 | 启动“服务研究”和“界面研究”两个子 Monad。 |
| 约 9 秒 | 主 Monad 请求暂停界面研究，随后收到 paused 确认。 |
| 约 13 秒 | 补充的外部服务保护要求发送给服务研究，并进入其模型上下文。 |
| 随后 | 服务研究依次汇报 finding、progress，最终完成。 |
| 约 92 秒 | 主 Monad 将服务研究的发现发送给仍暂停的界面研究。 |
| 约 94 秒 | 主 Monad 恢复界面研究；后续模型请求可见转交的内容。 |
| 随后 | 界面研究汇报 finding 并完成，主 Monad 看见两份实际终态结果后汇总。 |

离线验证检查了真实模型、同一主任务三次输入、父子身份、两个完成结果、暂停确认、暂停期间无新工具启动、补充要求进入服务上下文、服务报告先于转交、转交内容进入界面上下文、转交先于恢复、两份终态对主任务可见。

这些检查针对通信与时序，不证明被转交的文字内容正确。暂停是安全边界上的协作暂停，不是对正在执行的任意系统操作做强制冻结。

## 人工源码复核：原报告不能直接采纳的部分

### 1. 没有“接管已有外部 stdio 进程”的实现

[MCP open](../crates/smallhead-mcp/src/manager.rs) 的 command 分支实际创建并 spawn 一个新进程，配置 kill_on_drop，并交给 TokioChildProcess 管理。把相同命令写入配置不等于接管一个已有进程。

原报告的“外部 stdio 会被接管并误杀”推论没有依据；其建议关闭默认清理、仅补 external 字段，不能实现已有 stdin/stdout 的接入，反而可能留下无人管理的子进程。这些建议未实施。

### 2. HTTP 断开不等于远端没有任何变化

本地 MCP manager 没有向远端 OS 进程发终止信号的路径。但使用的 rmcp 3.5.1 Streamable HTTP transport 在关闭时可能调用 delete_session，清理远端 MCP 会话。

因此原报告“只关本地 reqwest client”“不向远端发任何请求”“远端完全不受影响”的表述不成立。应区分远端服务进程与协议会话。

SDK 复核位置：本机 Cargo registry 的 `rmcp-3.5.1/src/transport/streamable_http_client.rs`，约 1807 行的 session cleanup 分支。

### 3. 已有字段与证据范围没有认真核对

ConnectionSnapshot 定义就在 manager.rs 内，不能报告为“定义未读、位于范围外”。原报告还使用不准确的估算行号，并将 desired_running 这种运行意图扩大为归属证据。

可确认的事实：

- [ServiceManager](../crates/smallhead-process/src/service.rs) 的 stop_inner 拒绝停止 external 和 Unknown 服务；forget 清登记，不发进程信号。
- [MCP manager](../crates/smallhead-mcp/src/manager.rs) 与 ServiceManager 分开管理；command 创建本地 stdio 子进程，url 使用 HTTP transport。
- [MCP 面板](../crates/smallhead-tui/src/mcp_services.rs) 显示传输类型、连接状态、工具数和错误；其 shutdown 调用 disconnect_all。
- 当前连接类型已足以区分现有实现中的“本程序创建的 stdio 子进程”和“HTTP 连接”；尚未证明必须新增 external 字段。

若后续改面板，可先准确说明这两类连接的生命周期，保留已拥有子进程的清理；不应以这份模型报告为依据引入虚构的外部 stdio 支持。

## 本轮修复与可复现入口

- delegate 控制参数说明明确 task_id/run_id/attempt_id 要求；遗漏或身份不匹配时返回本任务可操作的登记信息，提示修正原动作。没有放宽身份校验。
- 子 Monad 区分 token 预算超额、max_tokens 截断和 content_filter，记录 monad_budget/rejected_response 诊断及实际用量。
- 增加 opt-in 的真实 Runtime 测试和离线事件核验脚本。真实模型测试默认忽略，显式运行会消耗 API 额度。

```sh
SMALLHEAD_MONAD_LIVE_FOCUSED=1 \
SMALLHEAD_MONAD_LIVE_OUTPUT=/tmp/smallhead-monad-live-new-run \
cargo test -p smallhead-tui monad_live::monad_live_collaboration \
  -- --ignored --exact --nocapture

node scripts/verify-monad-live.mjs /tmp/smallhead-monad-live-new-run
```

输出目录必须是新目录。可以仅对已有 C 轮目录运行核验脚本，不消耗模型额度。

回归：core、xerj、TUI 的 lib/bins/tests 串行运行，361 个测试通过、3 个忽略；真实 C 轮测试另行显式运行通过。core/TUI all-targets 严格 Clippy 通过。

## 下一步优先项

1. 改善子 Monad 的本地源码定位与搜索，减少反复分页大文件和无关联网，提高预算利用率。
2. 汇报明确区分源码事实、推断、建议和未验证项；主 Monad 在把子报告提升为“已验证事实”前，对关键结论做定点源码复核。
3. 继续改善定向动作的回执与参数引导，降低模型抄错身份和超出等待上限的频率。

这次尚未强制落实第 2 项；源码人工复核揭示的问题应保留为下一轮质量验收的用例。
