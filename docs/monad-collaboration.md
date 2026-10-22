# Monad 双向协作协议

实现日期：2026-10-18。保持主、子 Monad 语境；子 Monad 复用同一个 AgentLoop 实现，但拥有独立 history、owner、预算和宿主选择的工具。此轮不引入 Lens 组合机制。

## 通讯与动作

| 发起方 | 动作 | 行为与回执 |
| --- | --- | --- |
| 主 | `monad_delegate discover` | 查看角色、可用工具、动作及预算 |
| 主 | `start(tasks)` | 提交 1–4 个独立任务，立即返回 task_id / run_id / attempt_id |
| 主 | `dispatch(tasks)` | 兼容入口：等整批终态或新协作事件，最多 30 秒；返回不等于整批完成 |
| 主 | `status` | 查询本轮自己的运行记录、通讯状态和事件 |
| 主 | `wait(after, timeout_ms)` | 等新事件或收取终态；1–30000 毫秒，超时只结束等待 |
| 主 | `send(..., message)` | 补充信息或修正方向，进入子任务的 steering 队列 |
| 主 | `reply(..., request_id, message)` | 精确回复一条待对齐请求；旧编号或错任务不能解锁 |
| 主 | `pause(...)` | 发出协作暂停请求，等安全边界确认 |
| 主 | `resume(..., message?)` | 解除显式暂停，可附带新指示；不会替代 reply |
| 主 | `cancel(...)` | 请求取消并清理；还需查询真实终态和 cleanup |
| 子 | `monad_report(kind=progress, message)` | 报告重要阶段进展，不代表完成 |
| 子 | `monad_report(kind=finding, message)` | 报告影响其他工作的发现，写出来源与不确定性 |
| 子 | `monad_report(kind=blocked/alignment_request, message)` | 创建 request_id，在安全边界等待主 Monad 对应回复 |

`...` 表示必须带回执中的 `task_id`、`run_id` 和 `attempt_id`。定向 status / wait 也校验完整身份。主身份来自宿主 ToolContext.owner；子工具无法从参数选择收件人、冒充父任务或另一 attempt。每项委派明确 objective、context、completion_criteria；子任务不隐式继承主完整对话。

例子：主派发两项独立研究；其中一项发现两个版本互相矛盾，通过 alignment_request 提出问题并等待。主读取其证据，必要时询问用户，然后使用返回的 request_id 回复。另一项可继续运行。最终主读取各项结果和 transcript artifact，综合验收后才答复用户。

## 状态与确认

运行状态由 MonadRuntime 管理（排队、运行、验收、清理、终态）；通讯状态单独管理，两者不能互相替代。

| 通讯 phase | 含义 |
| --- | --- |
| `running` | 无协作等待；不证明当前正在占用 CPU，可能还在运行队列 |
| `pause_requested` | 已收到显式暂停请求，尚未到安全边界 |
| `paused` | 已到安全边界并确认显式暂停 |
| `waiting_alignment` | 有未回答请求；pending_request 给出准确编号 |
| `ended` | worker 已结束且运行记录已写入；成功、失败、取消须另看运行结果 |

显式暂停与待对齐请求独立存在。先 pause 再 reply，仍需 resume；先 resume 但未 reply，仍等待对齐。消息被接受仅表示入队；messages_delivered 和 delivered_steering_ids 表示哪些 steering_id 已进入子 history，不证明模型已理解或同意。阶段报告的接受回执也不证明主已阅读或验收。

暂停是协作暂停，在下一次模型请求、响应提交、工具调用或最终返回的安全边界检查。正在运行的模型/工具调用允许完成；不会冻结 OS 进程，不回滚已经发生的动作。补充指示会使尚未启动的旧工具调用以明确的 skipped 结果配齐，之后再让模型按新信息推理，不重放旧工具。

## 事件进入上下文

CollaborationHub 提供有界、非消耗的事件历史。事件含宿主绑定的父/子身份、attempt、全局序号、阶段和可选 request_id。每个主任务有独立 cursor，读取 `after` 之后的事件；保留响应 cursor，避免重复处理。

主 AgentLoop 自动在完整的 assistant/tool_result 协议边界分页收取事件，作为标明来源的引用资料放进 history。结束事件会附上同一 attempt 的运行结果、证据引用、清理状态及错误，主无需仅凭“已结束”猜测产出；status/wait 仍可再次查询。等待模型时有新事件会唤醒循环，放弃尚未提交的旧响应并重新推理；不会把半个 tool call 提交进去。事件及引用文本不成为系统指令或新增用户授权。

主准备最终回复时仍有活跃子任务，会收到宿主收尾提示，要求 status/wait 或取消并确认清理。预算耗尽则明确失败并触发父子取消流程，不能静默遗留一个等待回复的子任务。

## 生命周期、预算与限制

- 通讯是进程内机制，没有持久邮箱、重启恢复或跨回合后台任务。主回合结束或被取消，父 token 会取消其子任务；宿主等待 owned worker 清理。
- 暂停和等待对齐仍占运行槽位，任务总 wall-clock timeout 继续计时。默认 research/reviewer 每项 180 秒、40 请求步骤、64000 usage tokens；不是无限期等用户的流程。
- 默认 research/reviewer 仍是只读工具集，不开放 shell、写文件、服务控制或继续委派。变更目标通过 send 表达，不悄悄改写原 handoff、身份或权限。
- 通讯记录和消息大小有上限。拒绝消息必须明确报错；待回复、恢复、取消和终态通知保留必要容量，避免因报告填满历史而永久锁住任务。此机制不提供无限轮对话。
- 等待超时不会取消任务；取消不等于清理成功。检查 runtime 的 terminal、result、cleanup、error。远端副作用无法由本地取消撤销，失败不自动重放。
- `Completed` 只代表宿主绑定及已配置 validator 通过；主仍需验收证据与业务结论。完整子 history 保存为 artifact，结果正文按现有阈值裁剪。

## 代码入口

- [collaboration.rs](../crates/smallhead-core/src/monad/collaboration.rs)：Hub、状态、请求关联、边界暂停与容量约束。
- [delegate.rs](../crates/smallhead-core/src/monad/delegate.rs) / [delegate_control.rs](../crates/smallhead-core/src/monad/delegate_control.rs)：主工具、控制身份校验与主提示词。
- [report.rs](../crates/smallhead-core/src/monad/report.rs)：子工具及协作提示词。
- [agent.rs](../crates/smallhead-core/src/monad/agent.rs)：绑定独立 AgentLoop、报告工具及 steering inbox。
- [agent_loop.rs](../crates/smallhead-core/src/agent_loop.rs)：边界检查、主事件收取、模型等待唤醒与结束检查。
- [runtime/mod.rs](../crates/smallhead-core/src/monad/runtime/mod.rs)：execute / submit 共用 worker、wait_task、定向 cancel 和终态后关闭通讯。
