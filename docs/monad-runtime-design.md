# Monad Runtime：当前实现与边界

本文对齐 2026-10-18 当前工作区实现，沿用 Monad 语境，不新增 multiagent 抽象。下文区分已实现机制与未提供的保证，不记录正在运行的测试通过数。双向通讯与暂停机制见 [Monad 协作协议](monad-collaboration.md)。

## 1. 模块与执行入口

| 模块 | 当前职责 |
| --- | --- |
| [monad/runtime/mod.rs](../crates/smallhead-core/src/monad/runtime/mod.rs) | MonadRuntime：准入、记录、attempt fence、依赖前置、semaphore、owned worker、验收接口、取消与 timeout、清理状态 |
| [monad/agent.rs](../crates/smallhead-core/src/monad/agent.rs) | AgentMonad：独立 AgentLoop 适配，复用 Provider，隔离 history / owner / token，计量单任务预算，输出 findings 和 transcript artifact |
| [monad/delegate.rs](../crates/smallhead-core/src/monad/delegate.rs) | monad_delegate 宿主工具：批量独立 research / reviewer 任务，经同一个 runtime 内部并行 |
| [monad/collaboration.rs](../crates/smallhead-core/src/monad/collaboration.rs) | 父子身份绑定、阶段报告、请求关联、补充指示、协作暂停与恢复 |
| [monad/delegate_control.rs](../crates/smallhead-core/src/monad/delegate_control.rs) / [report.rs](../crates/smallhead-core/src/monad/report.rs) | 主 Monad 控制动作、子 Monad 报告工具和双方提示词 |
| [monad/registry.rs](../crates/smallhead-core/src/monad/registry.rs) | Monad 目录与独立实例工厂；不承担运行生命周期 |
| [monad_bridge.rs](../crates/smallhead-core/src/monad_bridge.rs) | 业务发现与路由，执行进入 runtime.execute |
| [xerj/monad.rs](../crates/smallhead-xerj/src/monad.rs) | project_research 经 runtime 执行；project_lookup 是直接后端读取工具 |
| [TUI runtime.rs](../crates/smallhead-tui/src/runtime.rs) / [app.rs](../crates/smallhead-tui/src/app.rs) | 长期持有 monads: Arc<MonadRuntime>，注入各 turn，取消与退出时等待子任务收尾 |

运行入口有等待结果的 `execute(child, handoff, parent_cancel, validator)` 和立即返回回执的 `submit(...)`。二者复用相同 owned worker 和准入机制。宿主先构造独立 MonadLoop 实例，runtime 验证绑定并准入；每个 attempt 只调用一次 start / step，具体推理循环仍由 AgentLoop 承担。确定性业务 Monad 不需要 LLM。

```text
主 AgentLoop -> monad_delegate / monad_bridge / project_research
             -> 同一 MonadRuntime.execute / submit -> owned worker
             -> start / step -> binding + 调用方 validator -> 记录结果
研究 AgentLoop -> project_lookup -> backend::run(map/search)，不再创建 Monad worker
```

ToolContext 已有 monad_runtime、cancellation、owner 等字段。TUI 显式注入自身 runtime；独立工具未获注入时可回退进程级 shared runtime，不能把该 fallback 描述成会话隔离。parent_task_id 当前由宿主上下文 / owner 关联，不宣称已有完整 session 身份授权系统。

依赖方向保持 `smallhead-xerj -> smallhead-core`，宿主组合 core、tools、XERJ、MCP；core 不反向依赖这些可选工具 crate，也不新增反向依赖 crate。

## 2. 准入、attempt 与依赖

runtime 按 TaskId 保留最新 attempt 的 RunSnapshot，使用锁保护准入与记录更新：

- 同一任务运行或排队中再次执行会拒绝；相同或更小 attempt 不重放。Completed 任务不能重试。
- 非完成任务只有上次 cleanup 为 Finished 才允许更高 attempt；parent_task_id、monad_id、result_slot 不得改变。Unknown、失败或超时清理不允许直接重放。
- 同一父任务下，result_slot 持续归原任务所有；终态记录不静默淘汰。记录容量满拒绝新 TaskId，以免丢失去重与结果槽归属依据。
- 每次执行有独立 run_id；Claim.update 仅在记录 run_id 匹配时更新，防止旧 worker 覆盖新 attempt。MonadRun 保存原始 Handoff（含 attempt）、结果和 run_id，不改现有 MonadResult wire format。
- depends_on 必须在准入时已是 Completed，自依赖、未知或未完成依赖直接拒绝。没有后台依赖等待、拓扑调度、自动 DAG 执行，也不宣称跨父任务授权检查全部实现。

WaitingContext 结束当前 attempt，不满足完成依赖；补充上下文需宿主明确发起更高 attempt，并满足清理条件。没有自动重试或持久恢复。防重复限于保留的 TaskId 身份，不等于按业务内容去重或外部副作用 exactly-once。

## 3. owned worker、取消与清理

execute / submit 准入后创建父 token 的 child token，并 tokio::spawn 一个拥有 child、Handoff、Claim 的 worker。execute 外层 future 的 drop guard 负责取消 token；外层被 drop 时 JoinHandle 脱离等待，内层 worker 继续响应取消并 await cleanup，不直接 abort 子任务而跳过异步清理。submit 的回执被丢弃不取消 worker；任务仍由父 token、总 timeout 和显式 cancel_task 管理。wait_task 等待特定 run 的终态，等待超时不取消执行。

worker 等待运行 semaphore 后执行 start / step、强制 BindingValidator 和调用方 validator。timeout 使用单调 deadline，从 execute 入口计算，包含排队、执行和验收；业务完成后再检查取消 / deadline。取消或 timeout 返回宿主绑定的 Cancelled 结果，记录中分别保留 Cancelled / TimedOut；生命周期错误、验收失败和 panic 返回错误并保留对应状态。

非 Completed 路径先结束业务 future 的可变借用，再调用 child.cancel，使用独立 cleanup_timeout（当前 8 秒）。运行 permit 覆盖执行、验收和有界清理，清理结束后才释放。Completed 不统一调用 cancel hook；AgentLoop 正常结束自身负责回合资源收尾。

cleanup 单独记录 Finished / Failed / TimedOut / Panicked / Unknown。Finished 只表示 cancel hook 成功，不证明远端操作回滚；真正 owned worker 被意外销毁才由 Claim Drop 标记 Abandoned / Unknown，不能把普通外层 future drop 等同于清理未知。

runtime 提供 cancel_all 和 wait_idle：前者取消当前登记的 worker，后者等待 worker（含有界清理）全部结束。两者均不关闭未来准入，宿主需先停止新派发；wait_idle 本身不取消任务。实现使用 active worker 表与 Notify，未引入 Codex 的整套 TaskTracker / 关闭准入设计。同步阻塞代码与脱离管理的外部副作用仍不能强制停止。

[AgentLoop](../crates/smallhead-core/src/agent_loop.rs) 每轮将 ToolContext.cancellation 换成 child token，结束时取消该轮 token；TurnScope 的 Drop 在正常返回或 future 被丢弃时恢复父 token，避免复用 loop 时沿用已取消 token。Provider 请求使用独立 cancellation，不调用共享 Provider 的全局 abort 影响兄弟任务。

[execution.rs](../crates/smallhead-core/src/execution.rs) 的 stop_and_reap_owner 会先尝试停止本 owner 的所有非终态进程，再以共享 6 秒等待窗口逐一 wait_for，确认实际终态并汇总错误；不是仅发送 stop。AgentLoop 正常返回与 AgentMonad.cancel 使用此路径，Drop 中异步停止仍是兜底。服务有独立 owner，不因任务结束误停；终态确认也不等于外部副作用撤销。

TUI 的 monads Arc 跨 turn 保留，父任务结束后 wait_idle。app.shutdown 先取消当前执行并 cancel_all，再处理父 worker、wait_idle，随后保存状态并关闭进程 / MCP；在 Tokio 退出前等待，而非仅依靠 Arc Drop。主会话检查点不包含持久调度自动恢复。

## 4. 研究与审阅角色

AgentMonad 共享 Arc<dyn Provider> 的连接 / 模型配置，每次 start 建立全新 AgentLoop 和局部 history。输入来自 objective、inputs、relevant_context、constraints、completion_criteria，不复制完整父对话；owner 含 task_id / attempt，子 token 独立。结果只取本轮最后一条无 tool call 的 assistant 文本，失败或无最终文本不会回用旧答案。

TUI 对 research / reviewer 均只注入以下工具，主循环原权限不变：

| 工具 | 边界 |
| --- | --- |
| read | 本地读取 |
| artifact_read | 读取已有证据 artifact |
| web_search | 搜索与网络读取 |
| project_lookup | 对已准备的项目做 map / search |
| monad_report | 报告进展、发现、阻碍及需对齐的问题；仅绑定到其主 Monad |

两角色没有 shell、edit / write、service / process 控制、任意 MCP 或 nested delegate。此限制由 TUI 构造的真实工具清单落实；DelegateTool 的 read_tools 由宿主提供，不宣称 core 对任意外部宿主已实现通用强制 effect gate 或完整参数级只读沙箱。

project_lookup 在入口拒绝 map / search 以外操作，直接调用 backend::run，不再 runtime.execute。父 worker 在工具调用期间仍占运行 permit，若所有研究 worker 再通过 project_research 请求同一池的新 worker，会发生许可耗尽死锁；直接读取入口避免这一嵌套。索引不存在时由父 Monad 通过普通 project_research 准备，子任务不能 ensure / index / poll 或启动服务。

普通 project_research 仍走统一 runtime，保留原后端操作语义。“提交索引任务”“服务 Ready”不等于索引完成或研究完成；父 Monad 依据实际 poll 终态和检索证据判断。

monad_delegate 接受 1-4 项独立任务，经 submit 提交，实际执行受 runtime 并发限制。start 立即返回身份回执；兼容 dispatch 最多等待 30 秒，整批终态或出现可处理的协作报告时返回，running 不冒充完成。单项错误保留本项关联，不默认取消兄弟任务。需要前后依赖的研究由父 Monad 分批发起。外层 tool_call_id 关联批量工具结果，内部 TaskId / attempt / run_id 关联各次执行，不靠完成顺序绑定。主循环在协议完整的边界收取报告，结束前检查仍在运行的子任务；先收集结果或取消并确认收尾。

每项保存完整子任务 transcript artifact，通过 evidence_refs 返回；findings 正文按 UTF-8 字符边界裁到 6000 bytes，findings_truncated 标明截断，完整文本在 transcript。当前截断后还追加固定提示，因此 6000 是正文裁剪阈值，不是整个 JSON / 工具结果的字节硬上限。

## 5. 验收含义与资源预算

runtime 总先执行 BindingValidator，再调用 CompletionValidator 接口。默认 delegate、bridge 和 project_research 传入的仍是 BindingValidator：检查 task_id，以及 Completed 的 parent_task_id / result_slot / monad_id；其他结果 variant 通过宿主保留的 Handoff / run_id 关联。

这只验收 binding，不能证明 completion_criteria 达成、引用真实、覆盖充分或审阅结论正确。当前没有真正的默认领域语义 validators，父 Monad 负责读取证据、综合与最终验收；Completed 只表示当前执行通过已配置检查。不能把“依赖已 Completed”写成“依赖已通过完整领域验收”。领域 validator 是已有扩展接口，不是已完成能力。

| 当前默认 / 机制 | 实际含义 |
| --- | --- |
| runtime 并发 2、额外队列 64、记录 4096 | 运行 / 准入 semaphore 与记录容量；超限拒绝，不淘汰去重记录 |
| cleanup 8 秒 | 非 Completed 路径的独立 cancel hook 宽限期 |
| delegate batch 4、40 steps、180 秒、64000 tokens | 当前批量与单任务预算默认，不是全树总预算 |
| AgentMonad BudgetProvider | 请求前限制步骤与剩余输出额度，按 Provider 报告的输入 + 输出 usage 累计；超预算 / 截断 / 过滤阻止该响应继续成功执行 |
| usage_reported | 标明是否取得 usage；缺失报告不能证明零消耗或精确守住预算 |
| LoopBudget | 已有模型波次、总工具调用、连续错误、重复调用保护；与 BudgetProvider 的请求步骤限制分别执行 |
| history / artifact | 已有 history 压缩、大工具结果投影；AgentMonad 额外提供每项 findings 裁剪和完整 transcript |
| ProcessLimits | 已有进程活跃数、记录、磁盘、并发、输出与执行 timeout 限制；不代表全部网络动作已统一限流 |

runtime 容量与 cleanup 可经 MonadRuntimeConfig 配置；delegate 批量 / 预算目前是代码中的默认值，尚不能宣称已有统一用户配置入口。Budget.concurrent_io 字段存在，不代表 runtime 已实现通用 I/O semaphore。

token 保护是单任务、基于 usage-report 的预算机制。并发在途请求、输入 token 无预留、usage 缺失和取消后的服务端消耗，均使其不能提供 tree-wide 输入加输出 token 硬上限；不将输出 max_tokens 或汇总统计等同于全树硬约束。

## 6. 当前不提供的能力与验证范围

不提供 worktree 并行写、long-lived mailbox、递归委托调度、持久调度自动恢复、远端副作用回滚或自动重放。也不把 session 身份授权、统一 effect gate、自动领域 validator、完整 TaskTracker 树关闭与通用 I/O 配额写成已实现。

当前实现的验证重点是：重复 / attempt / 槽归属拒绝、run_id fence、外层 drop 后内层收尾、permit 覆盖 cleanup、token scope 恢复、owner 终态回收、TUI 生命周期、project_lookup 不嵌套占许可、角色工具清单、UTF-8 裁剪与 transcript、usage 缺失时的诚实报告。测试由主线程运行，本文不记录通过数，也不因源码已有测试就声称这些验证均通过。

## 7. 参考实现与精确来源

只借鉴模块、取消、资源、身份与结果隔离机制，不复制代码。参考行为与本项目实现分开说明，行号对应本地参考快照。

### Codex

快照目录 `/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72`，版本标识 `ab45264919aaeb8a421cc156f1a5459ef9d60b72` 来自目录名；目录无 Git 元数据，不声称已通过 rev-parse 验证 HEAD。

| 来源 | 位置 | 参考机制 |
| --- | --- | --- |
| [control.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control.rs:67) | 67-98 行 | 控制句柄与职责子模块分离，单线程推理不放入工具入口 |
| [multi_agents/spawn.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/tools/handlers/multi_agents/spawn.rs:55) | 55-139 行 | 工具解析、有效配置、控制层调用分开 |
| [multi_agents_v2/spawn.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs:131) | 131-159 行 | 从调用上下文生成来源与规范任务路径 |
| [control/runtime.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/runtime.rs:93) | 93-115、185-210 行 | Weak 回指、共享状态、TaskTracker 与关闭准入；本项目没有照搬整套设计 |
| [control/spawn_guard.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/spawn_guard.rs:45) | 45-90 行 | 半启动资源由 guard 拥有，失败触发清理 |
| [control/spawn.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/spawn.rs:872) | 872-893 行 | 初始输入被接收后 commit / disarm |
| [control/execution.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/execution.rs:22) | 22-29、70-76 行 | Drop 释放计数；本项目使用实际 semaphore permit |
| [control/target.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/target.rs:14) | 14-27 行 | 从注册调用者解析相对目标；解析本身不是完整授权 |
| [control/interrupt.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/interrupt.rs:14) | 14-50 行 | 当前任务中断与根 / 自身规则、卸载目标幂等处理 |
| [control/watch.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/watch.rs:14) | 14-63 行 | 初始状态快照、合并更新、Weak 避免订阅保活 |
| [multi_agents/wait.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/tools/handlers/multi_agents/wait.rs:123) | 123-218 行 | V1 等目标终态；等待超时不取消目标 |
| [multi_agents_v2/wait.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs:187) | 187-204 行 | V2 等邮箱 / 新输入；本项目不引入 mailbox |
| [control/completion.rs](/Users/weykon/Desktop/p/agent-reference-sources/20261004/codex-ab45264919aaeb8a421cc156f1a5459ef9d60b72/codex-rs/core/src/agent/control/completion.rs:107) | 107-140 行 | 控制层定向投递终态、queue-only / best-effort，不保证 exactly-once |

### Claude Code

非官方 source map 还原仓库的 [README](/Users/weykon/Desktop/p/claude-code-sourcemap/README.md:15) 标注 `@anthropic-ai/claude-code 2.1.88`，从 cli.js.map 的 sourcesContent 还原。已读取仓库 HEAD 为 `a8a678cb6244e6770e1e421767ff0987a1d95549`，不是 Anthropic 官方源码提交。

| 来源 | 位置 | 参考机制与取舍 |
| --- | --- | --- |
| [runAgent.ts](/Users/weykon/Desktop/p/claude-code-sourcemap/restored-src/src/tools/AgentTool/runAgent.ts:347) | 347-378 行 | 独立身份、消息数组、文件状态；借鉴局部上下文 |
| 同文件 | 465-478 行 | 子任务工具授权作用域；本项目 TUI 显式提供角色工具集 |
| 同文件 | 520-528 行 | 同步共享父 abort，异步默认不关联；本项目采用父子 token |
| 同文件 | 648-714、747-757 行 | 构造专用上下文后复用 query；对应 AgentMonad 复用 AgentLoop |
| 同文件 | 194-209、816-847 行 | 区分新建 / 借用 MCP client，清理 hooks、缓存和所属 shell task |
| 同文件 | 735-805 行 | 按子任务身份写 sidechain transcript，分离运行结果与记录 |
| 同文件 | 866-902 行 | fork 前处理未配对工具调用；本项目采用显式局部 Handoff |

runAgent 的 try 从 747 行开始，之前已有资源初始化；finally 内单步抛错也可能中断后续清理，不能由其 finally 外形推断全部资源路径都有保证。本项目的 owned worker、有界 cleanup、进程终态等待与 TUI wait_idle 是各自实现的具体机制，仍不等于远端副作用已撤销。
