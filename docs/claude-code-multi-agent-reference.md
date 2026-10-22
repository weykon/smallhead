# Claude Code 多 Agent 架构对照与 smallhead 补强建议

2026-10-17。状态：研究与讨论稿，未开始实现新的多 Agent 调度器。

相关提交：`4b39f40 feat(research): 接入 XERJ 项目检索 Monad 与受管索引链路`。本稿与该功能提交分开。

## 结论

smallhead 已经有主循环、Monad 协议、结果槽位、进程所有权、服务管理、上下文压缩和交互恢复这些基础。下一步最有价值的是让“委派一件工作”具备可验证的生命周期，再逐渐开放并行。增加角色名字本身不会带来可靠协作。

建议先实现受控的研究/审阅子 agent，再考虑可以长期交谈和自领任务的 teammates。用户继续自然对话，由主 agent 判断是否拆分；不要求用户管理 session，也不引入 world 或 Lens 的新定义。保持目前主 agent 的开放权限偏好。

## 参考来源及版本边界

官方资料在 2026-10-17 检索和阅读：

1. [How Claude Code works](https://code.claude.com/docs/en/how-claude-code-works)：主循环、工具、上下文与交互。
2. [Create custom subagents](https://code.claude.com/docs/en/sub-agents)：独立上下文、角色工具集、fork、嵌套与并发上限。
3. [Agent teams](https://code.claude.com/docs/en/agent-teams)：lead、teammates、任务依赖与 mailbox；官方仍标为实验功能。
4. [Hooks reference](https://code.claude.com/docs/en/hooks#taskcompleted)：完成前允许程序检查拒绝不合格结果。
5. [Building effective agents](https://www.anthropic.com/engineering/building-effective-agents)：从简单组合开始，按需要增加路由、并行和协调。
6. [Multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)：研究系统的委派边界和结果综合。它不是 Claude Code 内部架构说明，不能混同。

本地找到两份参考：

- `../claude-code-sourcemap/restored-src/src/`，仓库提交 `a8a678c`。README 明确说明它是从 npm source map 恢复的 **2.1.88 非官方研究版**，不代表原始内部仓库结构。本次主要读它。
- `../claude-code-2.1.88/`，当前提交 `04c8ef2`。README 表明它是带社区修改的逆向恢复项目，不能把其 stub、补丁或 README 完成度当成官方事实。

本地 Explore 配置禁止继续调用 Agent；当前官方文档已讨论可配置的子 agent 嵌套。因此本文不把“子 agent 永远不能创建子 agent”当作架构定律，也不照搬某个版本的固定并发数。上限应由宿主配置和资源预算决定。

本稿仅提炼机制，没有复制上游实现代码。下面的 Rust 组织方式、状态机和硬规则是对 smallhead 的建议，不声称 Claude Code 已逐项实现。

## 需要区分的三层

| 层次 | 要解决的问题 | smallhead 的对应关系 |
| --- | --- | --- |
| 主 agent | 理解用户、拆任务、处理补充信息、综合与最终交付 | 现有 AgentLoop / TUI Runtime |
| 子 agent | 在独立上下文中调查、判断、执行一个有边界的委派 | 未来用 MonadLoop 包装真实子 AgentLoop |
| 确定性能力 | 完成检索、计算或进程操作，返回实际结果 | 现有 project_research、calculation 等 Monad |

`project_research` 当前是第三层：每次 dispatch 做一个确定操作，没有内部研究模型循环。它可以成为研究子 agent 使用的工具，但不能因此宣称已经拥有多个独立推理的 agent。

普通子 agent 以任务结果返回主 agent；长期 teammate 需要额外的任务归属、消息送达、空闲与关闭协议。这两种生命周期应分别设计。

## 本地源码走读入口

以下行号对应本次读取的恢复版，不承诺随版本稳定：

| 文件（均相对于 `../claude-code-sourcemap/restored-src/src/`） | 关键位置 | 值得学习的机制 |
| --- | --- | --- |
| `tools/AgentTool/AgentTool.tsx` | 568 附近 | 为 worker 装配工具；同处还有 worktree 分支，不直接复用父上下文的所有可变状态 |
| `tools/AgentTool/runAgent.ts` | 520、700、816 | 同步与后台任务的取消控制器不同；构造子上下文；finally 清理自有 MCP、缓存和所属 shell 任务 |
| `tools/AgentTool/prompt.ts` | 约 90 起 | 委派说明应交代目标、背景、已知信息、范围和输出；不能预言尚未返回的结果 |
| `tools/AgentTool/built-in/exploreAgent.ts` | 约 75 起 | 把角色用途和工具可见集一起定义，短目录用于路由，长提示词用于执行 |
| `services/tools/StreamingToolExecutor.ts` | 105、129、210 | 解析参数后判断并发安全；非并发工具形成屏障；对中断和流式回退生成对应错误结果 |
| `utils/tasks.ts` | 541 起 | claim 时持锁重新读取，检查已有 owner、完成状态和未解决依赖 |
| `tasks/LocalAgentTask/LocalAgentTask.tsx` | 197 起 | 后台完成通知，原子检查并更新 notified 标记，避免重复通知 |
| `tools/TaskUpdateTool/TaskUpdateTool.ts` | 232 起 | 标记 completed 前执行 TaskCompleted hooks，校验失败则拒绝完成 |
| `utils/mailbox.ts` | 19 起 | 进程内消息队列、等待者和变更通知 |
| `utils/teammateMailbox.ts` | 137 起 | 跨进程文件邮箱、持锁重读和追加 |

不能只照抄：旧版 `teammateMailbox.ts` 的部分写入异常只记录日志，没有向调用者抛出错误；当前官方文档则明确要求写入成功才报告发送成功。对我们应规定：入队、落盘、消费是不同状态，持久化失败必须反馈，不能声称已送达。

## 对照 smallhead 的实际缺口

| 已有位置 | 当前事实 | 建议补充 |
| --- | --- | --- |
| `crates/smallhead-core/src/monad/protocol.rs:168` | Handoff 有 objective、inputs、constraints、completion_criteria、depends_on、budget、attempt_id | 让依赖、预算、尝试身份进入真实运行控制，而不只作为字段传递 |
| `crates/smallhead-core/src/monad/task.rs:165` | TaskSlot 校验身份、阻止同实例重复绑定 | 上层再保证跨实例的单一执行归属、重试和终态唯一性 |
| `crates/smallhead-core/src/monad_bridge.rs:33` | 每次调用创建 fixture registry，顺序调用 start/step | 保留简单路径；引入真正受管的子 agent 执行入口 |
| `crates/smallhead-core/src/agent_loop.rs:738` | 工具调用在循环中顺序 await；已有 steering 和历史修复 | 先保留顺序语义，再按经过审查的动作放行并发 |
| `crates/smallhead-core/src/monad/tool_gate.rs:182` | Gate 是基础工具级检查；没有识别参数副作用，也没有实际递减调用配额 | 在真实执行边界检查有效能力与预算，不能把描述性 bool 当作已实现的约束 |
| `crates/smallhead-core/src/plan.rs` | update_plan 校验计划格式，展示模型报告的进度 | 不直接拿进度快照代替调度记录或完成证据 |
| `crates/smallhead-core/src/execution.rs:61` | TaskExecutions 清理当前 owner 的后台进程 | 子 agent 使用独立 run owner，再由父子关系传播取消，不能全共用父 owner |
| `crates/smallhead-xerj/src/monad.rs:80` | Completed 表示这次检索操作返回，index 的 output.phase 可能仍 running | 显式区分“操作返回”“索引进程结束”“研究任务验收通过” |

本次没有找到接入当前 Monad 派发路径的统一 DAG 调度器。现有设计稿中的 Scheduler 是计划方向，不能以文档里的结构图当作已经实现。类似地，Budget 默认值是协议默认值，不等于主 AgentLoop 的实际运行上限。

## 优先落地的工程硬规则

这些规则应落实为类型、状态转移和测试；提示词用于指导模型，不承担唯一保障。

1. **任务有唯一执行归属。** 同一逻辑任务可以有多次尝试，但同一尝试只能有一个有效执行者。认领、检查依赖和写 owner 必须在一个原子操作里完成；不能先检查空闲再异步写入。
2. **只接受当前尝试的结果。** Handoff 已有 attempt_id，但当前 MonadResult 没有对称携带它。建议由宿主事件信封绑定 task_id、run/attempt、result_slot 和版本；旧尝试晚到的消息只能归档，不能覆盖新尝试或取消终态。
3. **只有运行器写任务状态。** 模型、worker、TUI 提交动作请求；单一所有者执行状态迁移。终态不可被普通晚到事件翻回 running。副作用不能承诺 exactly-once；应保证本地结果幂等接收，未知外部副作用先核对再重试。
4. **完成必须有适当证据。** Worker 返回的是结果候选。研究检查来源和覆盖范围；索引检查实际进程终态及报告；代码修改按受影响范围检查构建或测试。没有通用必要性时不强制全仓测试。主 agent 的“完成了”不是校验器。
5. **取消有归属和传播规则。** 用户取消父任务时清理仍隶属它的子运行和 shell 进程；常驻服务继续由 ServiceManager 管理。后台独立运行必须显式声明归属，不能因有了独立取消控制器就变成孤儿。
6. **并发按副作用决定。** read/search/map 可成为并发候选；ensure/index/stop、文件修改和未知 MCP 工具默认保守。`project_research` 同一个工具名下有多种动作，不能把整个工具一概标成只读。无约束 bash 也不能仅凭提示词保证只读。
7. **消息有身份、背压和失败反馈。** 消息包含来源、目标、任务/尝试和 message_id；有界队列满要反馈；重复通知去重。任务通知在主循环安全点消费，不直接从子任务回调里改父 messages。需要跨重启可靠送达时再增加持久化、确认和重投语义。
8. **子上下文以结果为边界。** Handoff 传必要上下文和 artifact 引用；原始搜索和日志保留在子上下文或 artifact。返回简短结论、来源、限制和完成状态，防止主循环压缩之后被大量子日志重新灌满。主 agent 对尚未返回的结果不做猜测。
9. **资源按任务树计账。** 并发数、递归深度、token、时限和重试都受宿主预算约束。区分等待用户与无进展停滞；重复同一失败调用应触发诊断，而非靠任意十轮上限结束所有正常工作。预算耗尽返回明确的不完整状态。
10. **写同一份代码有冲突策略。** 初期只并行研究和审阅。真正并行编辑时，明确文件归属或使用 worktree，结果由主 agent 集成和验证。worktree 的提交不自动进入主分支；dirty 工作区的未提交内容也不会自动出现在新 worktree 中，派发时必须定义基线。

## Rust 中怎样组织

继续沿用 MonadHandoff、MonadResult、TaskSlot 和现有执行服务，不另外设计一套业务概念。

建议让一个 Tokio task 持有任务图、运行归属和结果槽位，其他参与者通过有界 mpsc 提交命令。这样状态修改能集中审查，避免多个 Arc<Mutex<...>> 各写一半状态。这个 task 不在持锁或状态迁移中等待长时间外部 IO：先记录派发，再启动 worker，再消费其完成事件。

使用 CancellationToken 表达父子取消；每个 worker/run 的 shell 进程有独立 owner。用 Semaphore 限制活跃 worker，配额按共享任务树计账；等待用户不应占用不必要的 IO 并发名额。

用 enum 表达运行状态，状态迁移函数返回事件和下一步动作。JoinSet 或带身份索引的 JoinHandle 表负责回收实际任务，并将 panic、取消和普通错误转换成可观察的运行结果。Drop 可触发取消，但不能作为唯一的异步清理保证；退出路径必须有显式 shutdown 并等待清理。

ToolGate 的可见工具集与动作能力检查放在不同层：前者减少模型可用面，后者在真正执行前验证。例如只允许 research worker 使用 project_research 的 map/search/poll，ensure/index 由宿主或获准的主 agent 准备。需要保持开放的主 agent 不因此被全局收紧。

日志统一关联父任务、子运行、attempt、tool_call_id 和 execution_id；TUI 从运行快照与事件渲染，不反向充当状态权威。跨重启先恢复可确认的记录，对运行中但不可确认的 worker 标记 interrupted/unknown，不能用旧 PID 或模型摘要推断它还活着。具体存储方案留待 checkpoint 的讨论，不在本轮选定新数据库。

## 建议的实施顺序

### 第一段：任务生命周期与完成校验

先建最小受管运行器，补齐唯一归属、当前 attempt 识别、取消、回收和可观察终态。可以先串行工作，暂时不做 teammate 邮箱或跨进程抢任务。

把“操作返回”和“业务验收”分开；对不同 Monad 定义最小完成检查，不让自然语言 completion_criteria 假装已经可机器验证。保留未知/不完整结果，避免无限修复循环。

验收：重复完成只回填一次；旧尝试晚到被拒绝；取消后不会重新变成成功；worker panic 和 shell 子进程得到回收；服务不被误杀；提交索引不能提前完成研究任务。

### 第二段：研究与审阅子 agent

通过真实 AgentLoop 的 Monad 包装运行独立上下文，先提供两个用途：研究证据、独立审阅。初始并发可配置为 2，初始深度为 1；这是 smallhead 的保守起点，不是 Claude Code 默认值。

研究 worker 可组合 XERJ、read、websearch；准备索引由宿主路径完成，避免声称完全只读却在工作目录随意运行 shell。主 agent 自动决定是否派发，不把选择模式变成用户操作负担。

验收场景：“分别研究上下文压缩和服务恢复，最后说明两者在重启时怎么配合。”两个子上下文各自调查，返回来源和限制，主 agent 综合；用户中途补充要求能在安全点被接收，结果不会串任务。

### 第三段：并行实现与长期协作

当第二段稳定后，再增加文件归属/worktree、依赖任务推进、定向消息和关闭协议。只有需要 worker 互相补充信息时才上 teammate，而非为每次简单检索创建长期成员。此阶段再讨论持久化消息和任务恢复。

## 本轮已做与未做

已提交项目研究功能及最小工具注册依赖，并在干净提交快照验证了 12 项新增测试、宿主工具上下文测试和 TUI 的 --locked 编译。原工作区的 MCP、规格审阅、恢复等改动保持独立。

本轮完成网络资料检索、本地参考代码走读和本文建议；没有实现新的调度器、角色系统、能力限制或消息持久化，也没有改变主 agent 的默认权限。
