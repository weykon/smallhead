# CLI、进程与服务管理：统一执行契约

更新：2026-10-10。状态：基础 ProcessManager 已实现并经本次审核修复；完整执行、服务和界面能力仍按阶段推进。

> 2026-10-10 实现状态：S1 的 Unix/macOS 生命周期闭环、S2 基础预算、S3 基础服务管理、S4 Bash/Agent/TUI 接入均已落代码并有测试。当前仍缺 Windows Job Objects、主动脱组后代隔离、自动日志保留、CPU/内存硬限制、服务自动重启与依赖传播、PTY，以及声明式业务完成条件。下文同时保留目标设计；凡写“应当”的条目不自动等于已经实现。

> 已验证链路：ExecutionId 先登记；拒绝/spawn 失败/超时/取消/输出超限均可查询；Unix 进程组 TERM→KILL；stdout/stderr/events 有界读取；总磁盘与记录预算；跨重启执行恢复为 Lost；服务登记持久化并保守恢复；BashTool/ProcessTool 共享 manager；TUI F3/F4 查询和停止。真实 Laya/Qwen 仍取决于本机模型服务，仓库测试使用本地伪 HTTP 服务验证协议和生命周期。

依据：用户明确要求统一管理进程存活、质量和系统资源；Agent 能直接观察状态；所有命令与执行类型在 TUI 留痕；Agent 声明任务完成条件，用户可选中具体执行停止；长期 server 命令由服务管理器管理。本文与 [动态任务控制](task-runtime-control-design.md) 对齐，保留逐步实现、亲手验证和代码走读的方式。

## 1. 必须成立的约定

1. 每次执行尝试先登记 ExecutionId，再校验、排队或启动。拒绝、启动失败、超时、取消与成功都可查询，不能只有成功路径有日志。
2. 进程句柄由运行时持有，TUI 和 LLM 请求的结束不会意外丢失清理责任。任何创建进程的入口都走同一执行器。
3. Agent 声明目的、执行类型、生命周期和可检查的完成条件；运行时验证契约并根据观察更新状态。没有证据的“已经成功”不是状态更新接口。
4. 用户可直接停止具体 ExecutionId 或 ServiceId，不依赖模型先回复。停止请求幂等，用户停止优先于服务自动重启。
5. 后台能力与进程登记、停止接口同时交付；ServiceManager 在 ProcessManager 上管理服务，不自行另起一套 spawn/kill 实现。
6. 进程状态与业务结果分开：程序退出 0 可能仍未完成业务；服务启动任务完成后，服务可以继续运行；停止进程不撤销已发生的外部操作。
7. 保证有管理路径、有限等待和明确的失败状态；不能把无法确认的清理、健康或恢复写成成功。主动脱离进程组的程序、OS 强制杀死管理器等情况必须体现保证边界。

## 2. 职责与模块

| 层 | 职责 | 不由该层决定的事实 |
|---|---|---|
| Agent / CLI Monad | 理解目标、提出命令、完成条件和执行策略；根据结果继续工作 | OS 进程是否退出、端口是否属于本服务 |
| Task 层 | 管理 task/attempt、预算、依赖和业务结果 | 不直接持有系统 Child 句柄 |
| ProcessManager | 执行登记、排队、spawn、输出、取消、退出回收、资源管理 | 不自行重新解释业务目标 |
| ServiceManager | 服务身份、重复启动协调、健康、重启策略、使用关系 | 不绕过 ProcessManager 启动或终止进程 |
| TUI | 查询快照、展示事件、选择目标、发送控制操作 | App 内 Vec/Map 不是进程生命周期的唯一记录 |

建议增加独立 `smallhead-process` crate（名称可在实施时调整），公开类型请求和管理句柄；不依赖 smallhead-core 或 smallhead-session。core 中的 CLI 工具适配层把 TaskId 转换为执行关联信息；应用组合层注入管理句柄、工作目录、允许的环境和日志目录。会话层负责提供目录，不能让 core 为日志路径反向依赖 session。

既有 `TaskSlot` 保持单个 Monad 的绑定与消费职责。TaskGraph 可以稍后接入，本设计不以完整 DAG 调度器为前提。

### 2.1 已确认的 crate 切割与接入范围

本节是 2026-10-10 已确认的实施决定，开发时按此执行，不再把“所有工具是否进入 ProcessManager”留作开放问题。

```text
smallhead-permission ← smallhead-process ← smallhead-core ← smallhead-tui
```

箭头右侧依赖左侧。`smallhead-process` 可以依赖更底层、无 Agent 语义的 `smallhead-permission`，但不依赖 `smallhead-core` 或 `smallhead-session`。应用组合层创建一份共享的 `Arc<ProcessManager>`，注入需要创建或控制进程的适配器；不能每次工具调用各建一份管理器。

接入判断只有一条：会创建、维持、检查或停止 OS 进程的入口必须使用 ProcessManager。第一阶段的明确范围如下：

| 入口 | 是否接入 ProcessManager | 说明 |
|---|---:|---|
| `BashTool` | 是 | 从自行 spawn 改为把 Agent 输入转换成 `ExecutionSpec` 的薄适配器 |
| ServiceManager 启动/停止 Laya、Qwen | 是 | S3 在同一个进程执行能力上增加服务身份和 readiness |
| 使用 shell/program 的完成检查器 | 是 | 检查器本身也是一次受管执行，有独立 ExecutionId |
| `FastDecision` | 否 | 只调用已经可用的 HTTP endpoint；服务启动是另一条 ServiceManager 操作 |
| `MonadBridge` | 否 | 负责任务发现、派发和结果回填，不持有 OS Child |
| `Abort`、纯文件/内存工具 | 否 | 不创建系统进程，继续使用各自现有生命周期 |

权限分两层，不能混为一个判断：core 的 `ToolGate` 决定某个 Monad 是否允许使用 Bash/CLI 能力；ProcessManager 执行边界使用 `smallhead-permission` 检查具体启动规格。每次请求先分配 ExecutionId 并记入执行流水，再执行 permission 检查；拒绝结果进入 `Rejected`，因此 Agent 与 TUI 也能看见。BashTool 不保留一套可被 ServiceManager 绕开的独立命令检查。

S1 允许先把现有 shell 字符串检查接入管理器，但要把这是临时适配写清楚。增加 `program + args` 后，permission 应检查结构化启动规格及显式 shell 模式，不能长期依赖把所有参数重新拼成一条字符串。

## 3. 身份、所有权与持久记录

- `ExecutionId`：一条执行尝试；命令重试产生新 ID，关联原逻辑任务与 attempt。
- `ProcessId`：一次受管 OS 进程实例；OS PID/PGID 是它的属性。拒绝或 spawn 失败时没有 ProcessId。
- `ServiceId`：逻辑服务身份；重启保留此 ID，生成新 generation 和 ProcessId。
- 关联信息：session_id、task_id、attempt_id、tool_call_id、父 ExecutionId、触发来源（Agent / 用户 / 服务重启）。
- 运行时记录持有 Child、输出读取任务、取消信号、结束通知；可持久化记录只保存元数据和事件，不序列化 Child 或 Instant。
- PID 之外保存可获得的进程出生标识及管理实例标识。恢复时验证身份；仅凭旧 PID 或端口不发送停止信号。
- 服务 scope 明确为任务、会话或应用；服务使用关系单独登记。停止一个普通任务不会误停应用共享服务。

生命周期策略建议先支持：`task_bound`（任务取消/结束时按契约清理）与 `session_service`（跨聊天轮次，正常退出应用时清理）。需要跨应用退出存活的持久服务使用显式 `persistent_service`，必须依赖独立 supervisor 或可验证的托管/恢复机制；第一阶段不支持时应明确拒绝，不能用一个后台异步任务冒充持久守护进程。

## 4. 执行类型与 Agent 的完成条件

执行类型与底层启动方式分别表达：

- `command`：有限命令，可等待退出，也可返回句柄后续 wait；后台等待不改变它的所有权。
- `service`：持续服务，启动请求等待 readiness，长期生命周期交给 ServiceManager。
- 启动方式 `program + args` 优先用于明确程序调用；需要 shell 语法时显式 `shell`，携带 command。两者都经过同一登记、预算和日志。
- 非交互模式 stdin 关闭；交互 PTY 作为显式能力。尚未支持时返回 unsupported，不让需要输入的命令无限挂起。

Agent 提供 `objective` 和结构化 `completion_policy`，例如：

| 条件 | 运行时证据 | 完成含义 |
|---|---|---|
| exited，允许退出码列表 | 已回收的退出状态 | 有限命令达到约定退出条件 |
| artifact，指定产物检查 | 文件/结果检查器的实际输出 | 产物满足约定；通常与 exited 组合 |
| ready，健康检查定义 | 关联本服务实例的探测结果 | 服务启动任务完成，进程仍由服务管理器持有 |
| agent_assessment | Agent 的业务判断及证据引用 | 仅业务验收；不能替代 OS 生命周期或清理证据 |

当前实现状态：BashTool 已持久化 `objective` 与 `completion_policy`，并支持 `completion_policy.type=exited` + `allowed_exit_codes` 参与工具成功/失败判断。`artifact`、`ready` 与 `agent_assessment` 仍是目标设计，尚未接入 evaluator。

检查器由代码注册、输入有类型；多个条件明确 all/any 组合及失败条件。自定义 shell 验证仍是另一条受管命令，不能在检查器里暗中绕过执行器。任务已运行后变更完成条件要有 revision 与日志，不能静默降低标准。

资源上限由应用策略约束，Agent 可以申请预算但不能取消强制上限。无法满足执行类型、完成检查或硬资源要求时，在 spawn 前给出结构化拒绝。

## 5. 进程状态、取消与清理

主状态：`Queued → Starting → Running → Stopping → Exited`；附加 `Rejected`、`SpawnFailed`、`Lost`、`CleanupFailed`。`Exited` 保存实际退出码或信号，以及自然退出、超时、用户停止、预算触发等原因；无法取得退出码时显式 unknown，不填 0。

有限命令的流程：

1. 持久记录请求和校验结果，获取并发配额，建立输出落盘与取消信号。
2. 创建独立进程组，保存句柄，异步并发读取 stdout/stderr；等待退出、deadline、取消或资源事件。
3. 需要停止时标记 Stopping，发送温和终止；经过有限 grace 后，对仍属于本次执行的进程组强制终止。
4. 回收直接子进程，等待输出读取在有限期限内收尾，保存尾部输出及退出事实，释放配额。
5. 清理无法确认时保留 CleanupFailed/Lost、原因和后续观察能力。停止接口不能因为“信号已发送”就返回已停止。

保持 Tokio 实现；`Child::kill/start_kill` 不等于清理进程组，`wait_with_output(self)` 会消费句柄。需要拆出 stdout/stderr，执行器保留 Child。Unix 进程组、Windows Job 等由平台适配层处理，不宣称单一 Unix 实现具有跨平台保证。

取消 token 从 TUI/Task 传到管理器。TUI 可以结束等待，但管理器必须继续承担收尾；应用关闭也等待有限清理。调用 future 被 drop、父任务异常退出、重复取消、退出与取消同时到达都必须有明确行为。外层直接丢弃 main.step() 不应成为唯一取消实现。

普通进程组不能控制主动 setsid/逃离管理范围的所有后代。记录此限制；需要强进程树隔离的执行必须使用平台支持的隔离机制，能力不足时明确拒绝该保证。

## 6. 系统资源与输出质量

- 限制并发、队列长度、单命令运行时间、服务启动时间、停止 grace、管道收尾时间；等待配额的时间也有预算。
- 字节流持续消费 stdout/stderr，避免管道写满阻塞。无换行、超长单行、二进制/无效 UTF-8 都要有定义；原始日志与文本展示分开处理。
- 内存只保留有界头/尾、有限错误摘录和增量缓冲；同时限制行数与字节数，不能先 collect 完再截断。
- 日志使用 ExecutionId 唯一路径，不用 PID 作为唯一文件名；设置单次及会话容量、保留策略。到达硬输出预算时按契约停止并标记 output_limit，或明确记录省略区间；不承诺无限“全量落盘”。
- 保存输出截断/读取失败/落盘失败事实。必须记账而日志存储不可用时，拒绝新执行；已运行命令采用预先定义的受控收尾策略，不能静默丢日志。
- CPU、内存、磁盘和后代数量区分“观测值”和“硬限制能力”。采样监测不是硬上限；每个平台公开 supported limits，用户或任务要求不能实现的硬限制时明确拒绝。
- 常规 stdin、cwd、环境变量和凭据继承规则显式配置。当前 BashTool 清空环境后只放 PATH，需要核对实际构建工具对 HOME/TMPDIR 等的需求；不擅自复制全部宿主环境。
- 危险操作策略、搜索范围/成本策略分开分类；拒绝结果包含原因和可行替代建议。子串规则仅是启发式，不是完整 shell 沙箱。

## 7. ServiceManager

`ensure_service(spec)` 应返回复用已有实例、启动新实例或具体冲突，不能遇到端口占用就假定成功或直接杀占用者。

服务定义包含：稳定名称与 scope、启动规格、配置指纹、端口/endpoint、readiness 与周期 health 检查、启动 deadline、停止策略、重启预算、使用者关系。

- 同一身份的并发 ensure 串行协调，避免同时 spawn 两份；配置不同则返回冲突或显式替换计划。
- Managed（本系统启动）与 External（仅发现/连接）分别记录。外部服务能连接不等于本系统有权停止，不能按端口自动接管。
- `Starting/Ready/Degraded/Stopped/Failed/Unknown` 为服务状态；进程 Running 不自动等于 Ready。
- readiness 检查应验证预期服务类型/实例身份，不只检查端口开放。health 失败记录探测证据，重启遵循有界次数、退避和错误分类，避免重启风暴。
- 用户停止设置 desired_state=stopped，覆盖自动重启；重启必须是后续明确控制操作。
- 服务停止时列出受影响的使用者，停止结果向这些任务发出 dependency_unavailable 事件，不能继续使用陈旧 Ready 快照。
- 正常关闭、管理器崩溃和重新打开应用分别处理。恢复先读取登记并探测身份，未知实例标记 Unknown；不能假装恢复了已丢失的 Child 句柄或退出码。

Laya/Qwen 是第一组服务案例：未启动则 ensure 后等 ready；已健康运行则复用；不相关进程占端口则返回冲突；用户选中具体 ServiceId 停止后不自动复活。

当前已实现 `ServiceManager`，服务名称充当稳定 ServiceId。Laya/Qwen 预设由同一 `ProcessManager` 启动脚本，使用 loopback HTTP JSON identity 做 readiness；FastDecision 在请求本地 endpoint 前先 ensure。服务登记写入 `services.json`，重启后先标为 Unknown：身份健康检查通过则按 External 复用且无停止权，失败则须 `service_forget` 后显式重建。自动重启、退避预算、使用者关系和 dependency_unavailable 事件仍未实现。

当前链路是：调用方需要快速决策服务 → `ensure_service` → 复用健康实例或通过 ProcessManager 启动 → readiness 成功 → FastDecision 发 HTTP 请求。readiness 成功表示“启动服务”这项 Task 完成；服务进程仍继续运行并由 ServiceManager 持有。

## 8. Agent 与 TUI 的共同接口

建议由一个 CLI/进程工具入口按需发现能力，操作包括：

- `execute(spec)`、`inspect_execution(id)`、`list_executions(filter)`。
- `read_output(id, stream, offset, limit)`：有界增量读取，返回下一偏移、截断与退出状态。
- `wait_execution(id, deadline)`、`stop_execution(id)`。
- `ensure_service(spec)`、`inspect_service(id)`、`list_services(filter)`、`stop_service(id)`。

工具调用返回结构化 ID、真实状态、原因、日志引用及可用后续动作。完成/失败/健康变化产生通知，由主循环在安全边界消费；高频 stdout 不逐条塞入 LLM 上下文。主循环未运行时仍能查询最新快照与未消费事件，避免只靠易丢失的内存通知。

需要用户明确决定时，Agent 通过 `ask_user_choice` 发出结构化 `ChoiceRequest`，TUI 渲染为按钮卡片，用户选择后以 `ChoiceResponse` 返回工具调用。选项携带 `intent`，不能让 Agent 仅凭自然语言按钮文本猜动作。当前实现为进程内 broker，适合 TUI 会话；跨会话持久化 choice 属后续任务。

TUI 提供执行/服务列表，选择一项查看命令类型、原命令或 program/args、cwd、目的、owner、PID、状态、耗时、完成条件、退出原因、输出与日志引用。`停止选中项` 按钮与 Agent stop 工具调用同一个管理接口；不借助一次 LLM 请求才能停止。

Esc 的语义按焦点区分且在界面提示：输入框用于清除/返回；进程列表上取消选择或关闭详情；具体停止使用明确按钮/快捷键，避免一次误按同时取消聊天和服务。现有 Ctrl+X 仍表达取消当前聊天任务，传播到 task_bound 执行，保留其他 scope 的服务。需要更改快捷键时另行更新交互文档。

所有入口的执行都进入同一结构化事件流：用户发起、Agent 工具调用、检查器命令、后台启动及自动重启，包含校验拒绝和 spawn 失败。按 ExecutionId/ProcessId/ServiceId/TaskId 可串联。原始命令可查，凭据通过独立受控环境传入；显示和导出遵守项目的敏感值处理规则。

现有 TUI 内存 3000 条事件不是全部历史。持久执行日志是事实来源，内存窗口只是当前显示缓存；TUI 必须可按 ID 查询已经淘汰的记录，并明确日志保留期限与删除状态。

## 9. 分阶段验收，每阶段可亲手运行

| 阶段 | 交付 | 必测场景 |
|---|---|---|
| S1a 结构与所有权证明 | 已实现 | 成功、非零退出、spawn 失败、permission 拒绝；父调用 future 被丢弃后仍可 inspect/stop；超时清理进程组及管道后代；重复 stop 幂等 |
| S1b 生命周期收口 | Unix/macOS 已实现 | 忽略 TERM 后升级 KILL、自然退出与 stop 竞态、stdout/stderr 有限 drain、CleanupFailed/Lost；Windows Job 尚缺 |
| S2 输出与资源 | 基础版已实现 | 大量/无效 UTF-8 输出、输出预算、磁盘与记录配额、恢复查询；自动 GC 与 CPU/内存硬限制尚缺 |
| S3 服务 | 基础版已实现 | 重复/并发启动、外部占端口、健康超时、用户停止不复活、登记恢复与身份复核；自动重启与依赖传播尚缺 |
| S4 Agent / TUI 完整体验 | 执行控制已实现 | Bash/ProcessTool、事件、F3/F4 停止、输出查询、共享服务；完成条件、PTY、TaskGraph 事件尚缺 |

S1a 与 S1b 可以是两个独立 commit，便于代码走读和亲手验证；只有两批全部通过才称为 S1 完成。第一批不能只验证“返回了 timeout”：父 future 被丢弃和进程组后代清理决定 Child 究竟由 Manager 还是调用者持有，属于结构验收，必须放入 S1a。S1/S2 是可靠执行基础，S3 的后台能力不能先于最小管理与停止能力开放。界面可按阶段接入基础列表，完整 DAG、任意暂停/恢复和外部操作回滚仍属于后续任务控制设计。

测试不仅断言工具返回错误：需要验证受管进程及管道后代不再执行、无持续写入、输出读取正常收尾。不要单凭短暂 ps 结果推断整个进程树清理完毕。真实 Laya/Qwen 测试与不依赖模型的本地脚本测试分别记录。

## 10. 给开发 Agent 的实施约束

后续开发维持 Tokio、异步取消与平台进程组处理；不复制线程轮询，不依赖 Bash 字符串拼接的 nohup wrapper，不以尾部 `&` 猜测生命周期，也不把 App Vec 当作管理器。下一优先级应从自动保留、服务依赖传播、完成条件、PTY 和 Windows 隔离中按实际用户场景选取，不能把其中任何一项混称为简单“execute CLI 收尾”。

工具适配、执行器、服务管理、日志存储分别保持职责清楚；注入共享运行时句柄，不为每次工具调用新建独立管理器。是否需要 dyn trait 由实际替换点决定，不为所有模块先加 trait。所有阶段的文档需区分“已实现、模拟测试、真实服务验证、尚未支持”。

已落地顺序为：`smallhead-process` 基础类型 → Manager 持有 Child → permission 与 Rejected → S1 生命周期测试 → S2 输出/磁盘/并发预算 → BashTool 共享 Manager → ServiceManager 与 Laya/Qwen → TUI 控制。FastDecision、MonadBridge 和其他不创建进程的工具仍不直接进入 ProcessManager；它们只在需要本地服务时通过 ServiceManager 确保依赖。
