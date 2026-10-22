# 动态任务与 TUI 控制设计：改计划、暂停、停止及恢复

> 2026-10-09 补充：CLI 进程和长期服务的所有权、完成条件、用户停止、资源管理及日志链路，统一见 [CLI、进程与服务管理设计](cli-process-service-design.md)。TaskSlot 不承担 OS 进程所有权；后台能力必须与登记和停止接口一起交付。该补充为需求设计，尚不表示相关运行时已经实现。

> 状态：设计稿，尚未实现。2026-10-08。
> 优先级：用户功能级别 P1，作为后续功能规划；当前不进入实现。
> 基线：`milestone/task-slot-v1`，提交 `e77a287`；TUI 在当前工作区另有未提交实现。
> 本文把任务图与交互控制整合为下一阶段运行时设计，不扩展 TaskSlot 的局部绑定职责。

## 0. 当前工作重心与进入下一阶段的条件

用户已明确：本需求属于用户功能级别 P1。当前优先回到核心模块，通过亲手运行测试场景和代码走读，理解已经实现的能力，再安排下一环节。本文的阶段拆分用于保存设计，不代表立即开始开发调度器、暂停/恢复或外部效果管理。

本阶段的学习与验证顺序：

1. 亲手运行 monad_demo 的离线模式，观察第 3 段的局部等待/补充和第 5 段的完整发现、派发、结果回填。
2. 沿一次 calculation 任务阅读：工具调用参数 → bridge → registry 工厂创建实例 → handoff → start/TaskSlot 绑定 → step → MonadResult → 工具结果回到主 Agent。
3. 对照局部生命周期测试理解重复绑定、重复执行、等待后再次 start、取消和失败清理；区分“测试通过”与用户已经理解运行过程。
4. 理解大模型与快速模型在当前链路中的决策位置，以及计算 fixture 实际由程序执行的边界。

另一个开发 agent 正在整理原有两千行左右的 Monad 文件。当前走读以类型和职责为入口，待拆分后再更新文件位置；拆文件本身不意味着增加任务调度能力。进入 P1 实现前，由用户完成亲手体验与讨论，确认现有行为和下一步需求。此处的 P1 表示用户功能优先级，不与旧文档中 runtime 的阶段编号混用。

## 1. 当前能力与边界

TaskSlot 已统一四个 Monad 的校验、绑定、单次消费和清理。MonadBridgeTool 当前现场创建子实例、顺序执行并返回；没有保存跨轮次的任务图。TaskSlot.take 后为空，仅表示绑定已移交给执行代码，不能用它判断全局任务是否完成。

TUI 的 `runtime.rs` 每轮创建 MainMonad，`tokio::select!` 在完成、取消或超时之间选择。取消会丢弃本轮执行 future，恢复本轮前的模型消息，保留诊断日志；外部副作用不因此撤销。`app.rs` 当前 Esc 清空输入和退出日志焦点，Ctrl+X / `/cancel` 取消整轮等待。会话 `/load` 只恢复消息历史。以下 task 控制、持久化与恢复 API 均为新增设计，不能当作当前已有功能。

## 2. 一个贯穿的体验场景

用户：并发查 A、B 的报价；选择便宜的一个，用我稍后提供的数量生成订单草稿。

1. 主 Agent 提交 A/B 两个独立任务；调度器限流并发，完成后合并。
2. 草稿任务显式引用报价结果，缺数量时 WaitingInput；TUI 展示待补充字段。
3. 用户选择草稿任务，输入数量；系统关联到具体 wait_id，验证后恢复。
4. 用户要求改为只考虑 B；生成计划修改提案，保留 B 的有效结果，取消不再需要的 A 分支或使其结果失去本计划的使用资格。
5. 另一个明确的提交订单任务调用外部接口。如果超时，进入效果核实流程，不把“未收到响应”当成“订单未创建”。

先用查报价/创建订单的本地模拟适配器验证；计算 fixture 可演示图、数据引用和等待，但无法揭示外部副作用的不确定性。

## 3. 对象层次与唯一所有者

| 对象 | 职责 |
|---|---|
| SessionId | 一段用户对话，关联多个工作运行 |
| RunId | 用户的一项工作，可跨多次模型请求和等待；不同于单次 HTTP 请求 |
| PlanRevision | 该 run 的计划版本，追加修改，不覆盖历史 |
| TaskId | 一件逻辑工作，跨恢复或执行重试保持身份 |
| attempt_id | 同一 task_revision 的执行尝试，初次为 1，重试递增；新 task_revision 从 1 开始 |
| task_revision | 输入或任务定义变化后的版本；旧结果不能匹配新定义 |
| WaitId | 一次缺输入或人工决策的等待事项 |
| OperationId | 一个外部业务动作的身份，跨网络重试保持不变 |

`PlanSpec` 表达待执行的任务与依赖；`TaskRecord` 表达真实执行事实。调度器单一 actor 持有 TaskGraph、结果、等待记录和执行句柄；TUI、Agent、工具适配器通过命令和事件交互。TaskSlot 仍在单个 Monad 内使用。诊断事件适合展示，不能作为可靠任务存储；状态在事务提交后发出带序号的事件。

全局 plan revision 用于修改计划的并发校验；任务是否还能接受结果，依据该任务自身 revision、attempt 和停止状态判断。新增无关节点不应让其他运行中任务的结果全部失效。

## 4. 状态分开表达

任务执行状态：`Pending / Ready / Running / WaitingInput / RetryScheduled / Completed / Failed / Cancelled`。效果不确定的任务可进入 `Reconciling`，在核实后决定完成、重试或人工介入。

Run/子树的控制状态另设：`Active / PauseRequested / Paused / StopRequested / Stopped`。Task 可以已经 Completed，而它所属 run 正在 Paused；不要让一个 status 字段同时表达这两件事。

外部动作另记录 `NotStarted / InFlight / Confirmed / Rejected / Unknown`。任务被停止时，Operation 仍可能是 Unknown。用户能看到“工作已停止，订单状态待核实”，而不是错误地显示“订单已取消”。

图的三种关系必须区分：parent 表示拥有和取消范围；depends_on 表示前置条件；input_refs 表示实际数据来源。取消沿拥有关系传播，不把所有下游依赖任务都视为自己的子孙；失去必需依赖的任务进入阻塞/失败处理，不凭空执行。

## 5. 并发、依赖与数据流

- 支持一次提交一批任务，也支持运行中追加节点。注册前校验唯一 ID、引用权限、输入 schema、无环依赖和执行预算。
- `depends_on` 只表示就绪条件。`InputRef { source_task_id, source_task_revision, output_path, target_field }` 明确从哪个已确认结果读取数据；目标类型不匹配时不派发。
- 业务适配器有有限并发上限；run 总体另有上限。任务为读操作且互不依赖，才作为默认可并发候选；冲突写操作按业务键串行。
- DAG 节点代表可执行的工作单元，节点内部允许 Agent 循环。动态追加新节点前仍要校验；追加不能改变已派发节点的输入快照。
- 用完成事件唤醒就绪任务；聚合任务声明“全部成功”或“允许部分结果”的策略，不让模型误把漏掉的结果当成全量完成。

## 6. 暂停与停止的精确语义

### 6.1 暂停：在安全边界暂停

初版 Pause 阻止范围内派发新任务、发起下一次模型请求和启动新外部动作。正在执行的一步默认跑到边界并记录结果；TUI 先显示 PauseRequested，边界达成后再显示 Paused。用户输入草稿保留。

不能冻结一个任意 Rust future 并承诺跨进程恢复。执行器根据能力声明选择：`finish_current_step`、支持 checkpoint 的 `checkpoint_and_suspend`，或只允许 stop。长时间不可暂停工作在 UI 明确显示“等待当前步骤结束”。等待输入时不占用执行并发许可；已有 WaitingInput 不必为了暂停再执行一次。

Resume 从记录的安全边界继续，不重放已确认完成的动作。暂停期间收到的结果可以登记，但不派发后继。执行超时、等待输入时限、整个 run 的截止时间分别记录；暂停是否冻结业务截止时间必须有明确策略，初版不冻结外部业务有效期。

### 6.2 停止：结束指定范围的继续执行

Stop 先登记 stop intent，停止新派发，向运行中的执行器发协作取消信号。待派发节点可立即取消；运行中节点先 StopRequested，收到执行确认后再结束。不支持协作取消的任务仍可晚到，记录到原 attempt 历史，但不得驱动已停止分支继续运行。

外部写操作一旦发出，停止本地 future 不等于撤销操作。Unknown 的动作必须保留核实记录和核实能力；已确认成功的写入若需撤销，另建补偿任务并遵循业务规则。Stop 不删除记录，不默认提供 Resume；用户要继续时明确创建新版本或新运行。

结束与停止事件竞态由 actor 串行裁决：若完成先提交，保留完成事实；若停止先提交，晚到结果仅登记历史和外部效果，不继续调度。状态转换必须检查预期 revision，不能盲目覆盖。

## 7. 改计划时如何处理旧工作

用户自然语言先由 Agent 转成 `PlanPatch`：追加任务、替换未执行定义、停用分支及更新数据引用。提出修改不自动修改事实；先验证影响范围和预期 plan revision，必要的用户选择通过 TUI 呈现。

| 原任务情况 | 默认处理 |
|---|---|
| 定义与输入均未变，只新增其他工作 | 原任务继续，结果仍有效 |
| 尚未执行、输入已改 | 新 task_revision，重新判断就绪 |
| 正在执行、输入已改 | 旧 attempt 标为被替代，请求停止；新版本独立派发，旧结果不能填进新版本 |
| 已完成、输入/执行定义未改 | 在范围和时效允许时引用原结果 |
| 已完成、输入改变 | 新版本重新执行，原结果仅保留历史 |
| 被移除且正在做外部写入 | 停止后续动作，并核实已经发出的操作；不能通过删节点隐藏它 |

重用判定至少涵盖输入指纹、执行器/业务版本、相关上下文版本、权限范围和 freshness 策略。网络报价有有效期，算术结果通常无时效问题；两者不能用相同的永久缓存规则。不可变结果记录包含产出 task_revision/attempt_id、证据和时间。旧计划可以保留结果；新计划通过显式引用决定是否采用。

PlanPatch 修改生产者结果后，依赖于该结果的下游都需重新核验输入绑定；已执行的下游不能被静默改写。初版只支持明确的追加/停用/替换操作，复杂重排给出影响预览后提交。

## 8. 外部操作超时：核实后决定是否重试

工具适配器声明 `Pure / ReadOnly / IdempotentWrite / NonIdempotentWrite`，并提供是否支持取消、幂等键和效果查询的能力说明。重试策略独立于当前 Budget；现有 Budget 没有 max_attempts，不能假设已经有此字段。

```text
写入前：保存 OperationId、输入指纹与准备执行的意图
  → 发请求（支持时传稳定幂等键）
  → 收到可验证成功回执：Confirmed，保存外部 ID
  → 明确拒绝：Rejected，按错误类型处理
  → 响应丢失 / 超时 / 进程中断：Unknown
       → 用 OperationId 或外部业务键查询
       → 确认已成功：返回原回执
       → 明确未执行：满足策略后重试
       → 仍不清楚：保留待核实状态，向用户解释
```

OperationId/幂等键属于逻辑业务动作，跨 attempt 不变；输入变化代表新的动作，不能复用原键。多个本地 worker 的去重需要数据库唯一约束及原子领取；仅内存 HashMap 不够。

Outbox 或执行日志记录意图与派发责任，但不能消除“外部成功、本地尚未记录就崩溃”的窗口。必须依赖外部幂等协议或对账查询；两者都不支持时，不承诺 exactly-once，不盲目重试写操作。纯计算/读操作可按错误分类执行退避重试，仍需检查过期 attempt 和总体预算。

## 9. 等待补充与恢复

`WaitRecord` 保存 run/task/task_revision、wait_id、缺失字段 schema、问题文本和截止时间。Task 返回 WaitingContext 时，调度器保存它并产生 input_required 事件；主 Agent/TUI 主动展示问题。

`SupplyInput` 必须指定 wait_id 并校验 revision。只有一个等待事项时 UI 可默认关联；多个事项同时等待时展示选择，避免“数量是 2”被填进错误任务。补充的是用户输入，不是自动执行重试：增加 task_revision、保持原 TaskId，新版本 attempt 从 1 开始，start 新 handoff。旧等待项关闭，旧 attempt 迟到结果不覆盖新输入；执行结果通过 (TaskId, task_revision, attempt_id) 关联。

当前 CalculationMonad 的 WaitingContext 要再次 start；初版恢复可以重新实例化并注入完整输入，不要求保存 trait object。未来有内部多步状态的 Monad 必须另定义 checkpoint/restore 契约。跨轮次恢复先用内存 run store；跨进程恢复再引入 SQLite 执行记录。

持久化恢复时：Completed 复用；Ready 重新调度；WaitingInput 重建 UI；PauseRequested/Paused 保留控制意图；中断的 Running 依据业务类型核实或重新执行。外部 Unknown 先对账。保存执行器版本和输入快照，防止换代码后把旧工作当成相同动作重跑。

## 10. TUI 接口与 Esc

初版目标：任务列表/详情可选择 task，聊天页可选择当前 run；右侧诊断日志继续提供完整请求和工具数据。选中日志事件不等于选中可控制任务，应有明确的任务选择和作用范围。

**Esc 优先级建议：** 有弹窗时关闭弹窗；有补全/详情临时层时先关闭该层；处于可控制 run/task 页面时打开控制菜单；空闲输入状态退出临时焦点。不要无提示删除用户草稿。Esc 本身不直接停掉任意任务。

控制菜单显示目标名称/短 ID、影响子任务数和当前外部动作状态，提供“暂停至安全边界”“停止此任务/整个工作”“继续”“返回”。初版所选 task 的控制范围为该任务及其拥有的子树，菜单写明范围；将来若支持只控制本节点，必须显式选项。主聊天页默认为 run 范围，任务详情默认为选中 task 范围。暂停请求发出后可转为停止，不能把第二次 Esc 隐式解释为强制 kill。

建议命令：`/tasks`、`/pause [task_id]`、`/resume [task_id]`、`/stop [task_id]`、`/input <wait_id> ...`。不带 task_id 时作用于当前 run。Ctrl+X 为当前 run 的 Stop 快捷键；显示“已请求停止”和随后确认事件。退出 TUI 与停止任务分开处理：本地 runtime 可选择停止并退出；有持久后台 runtime 时可以仅断开界面。真正 kill 应是单独运维动作，不能与 Stop 混同。

### 10.1 内部命令与事件（示意，非现有 API）

```text
ControlCommand {
  command_id, run_id, target: Run | Task(TaskId), expected_revision,
  action: Pause | Resume | Stop(reason) | SupplyInput(wait_id, patch)
          | ApplyPlanPatch(expected_plan_revision, patch)
}
ControlAck { command_id, accepted, reason, current_revision }
TaskEvent { event_seq, run_id, task_id?, task_revision?, attempt_id?, kind, payload }
```

UI 发送命令不等于执行完成；ack 表示已接收/拒绝，Paused/Stopped 由事件确认。命令 ID 去重；revision 冲突返回当前快照。键盘重复事件不会生成多次操作。重连先取 snapshot，再接 event_seq 后的事件；日志截断不应导致工作状态丢失。

暂停不能直接依赖 `&mut MonadLoop::cancel()`：step 正持有可变借用。Scheduler 必须持有独立控制句柄，执行器通过 channel/token 在安全边界响应；step future 由 worker 拥有，调度器保存句柄和任务身份。不要在 TUI 渲染/按键处理代码内 await 长任务或持锁 await。

## 11. 分阶段开发与验收

以下为后续 P1 开发顺序，待第 0 节的核心模块体验与走读完成后再安排。

1. **可观察的内存运行时**：唯一 run/task store、单 actor、typed 命令/事件；现有 bridge 提交真实 TaskRecord，TUI 显示列表和身份。取消语义先与旧 Ctrl+X 对齐并明确其范围。
2. **并发与输入引用**：批量/动态追加、DAG 校验、有限并发、结果注入和聚合；测试成环、类型不符、部分失败和不同完成顺序。
3. **等待与控制**：WaitingInput、精确补充、边界暂停/恢复、子树停止；测试两项同时等待、重复命令、完成/停止竞态、暂停后晚到结果和改计划后旧结果。
4. **外部效果恢复**：模拟订单适配器故意在“创建成功后”丢响应，验证只核实一次订单、原幂等键跨重试不变；不支持核实的写入保留 Unknown，不能自动重发。
5. **跨进程恢复**：SQLite 原子状态提交和执行日志，在派发前、外部成功后、完成登记前注入崩溃；重启后重建等待与控制状态。无需在前几个阶段宣称持久执行。

阶段验收都应有离线 demo 与 TUI 同一场景，打印/展示 run、plan revision、task revision、attempt、operation 和状态变化。模型只负责语义决策；直接程序测试守住状态转换和效果核实，不让模型随机表现成为协议测试的前提。

## 12. 现在可以亲手体验什么

以下均针对现有实现；两个 core example 已在当前工作区离线运行成功。

```sh
# 五段演示：协议、注册表、等待/补充、Main、完整业务发现与派发链路
cargo run -p smallhead-core --example monad_demo -- --mock

# 空表达式返回 waiting_context，再派发合法表达式完成；B/C 用固定模型脚本
cargo run -p smallhead-core --example waiting_context_demo -- --mock

# 当前 TUI：固定主模型脚本 + 真正执行计算 Monad
cargo run -p smallhead-tui -- --mock

# TaskSlot 与 Monad 生命周期测试（含重复取消满队列后执行新任务）
cargo test -p smallhead-core monad::
```

monad_demo 第 3 段展示同一实例等待后重新 start，但第二次 handoff 使用新 TaskId。waiting_context_demo Part A 的两次 dispatch 也创建不同子 TaskId。它们演示局部绑定和派发结果，不是持久恢复旧任务；测试 calculation_waiting_context_can_be_resubmitted 覆盖同一 TaskId 补充输入后的再次 start。

TUI --mock 不理解用户文本：输入“先算100+100，再算100乘100”后 Enter，观察固定流程。F2 看日志，`/filter monad` 看 main_start / discover / dispatch / return / main_complete 的交接与回填；`/filter route` 看执行路线；Ctrl+R 看完整模型请求。聊天快捷键实际为 Ctrl+F1。Ctrl+X 或 `/cancel` 取消整轮等待，Esc 当前仍是清输入/离开日志焦点。mock 算术很快，人工可能来不及取消；取消测试用受控等待 provider 验证，不能凭“没赶上按键”判断取消失效。

真实体验：`cargo run -p smallhead-tui`，需要 MINIMAX_API_KEY；`--prefer-fast=laya` / `--prefer-fast=qwen` 额外调用本机服务。真实模式可观察网络等待，但当前仍没有任务选择、暂停/恢复、动态依赖图和重启后的任务恢复。

代码入口：`examples/monad_demo.rs` 第 3/5 段、`examples/waiting_context_demo.rs` Part A、`monad/task.rs` 局部绑定、`monad.rs` fixture/Main 实现、`smallhead-tui/src/runtime.rs` 整轮执行和 select 取消、`smallhead-tui/src/app.rs` 按键。

本次验证中修复 waiting_context_demo 的离线 Part C：原先复用 Part B 已耗尽的 MockProvider 导致退出；现在给 Part C 独立脚本，并断言实际结果为 WaitingContext 而非 Completed。输出区分真实模型与固定脚本。两个离线 example 均完整运行成功；cargo test -p smallhead-tui 的 6 个测试通过，包含整轮取消/超时和消息一致性测试。尚未对真实模型及本机快速服务做交互验收。
