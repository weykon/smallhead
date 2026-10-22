# Lens 领域工具扩展

实现计划与契约，2026-10-19。目标：在 TUI 加载 Learning Lens 时，同时取得教学循环策略、领域提示词和已有 Rust 学习工具；卸载影响下一轮，数据库继续保留。

## 实施划分

1. 通用包加载：Lens 增加可选 extension，固定工具 schema、配置和领域提示词，校验名称与路径。
2. 通用工具宿主：通过 ExternalTool 注册本轮可用工具，以 JSONL 调用任意语言编写的 worker，不让 core 依赖学习业务。
3. 真实输入来源：AgentLoop 从初始输入与已消费的 steering 绑定输入身份、文本及来源，独立于模型/Lens 投影。A2A 为 peer，子 Monad 为 delegate。
4. 学习适配：同一份 Rust Domain 用于独立 CLI 与学习 worker，复用数据库、精确引用、checkpoint 和证据约束。
5. TUI/A2A 集成与验证：同一 LensSession 装配工具和策略；覆盖装卸、权限子集、真实来源、错误与取消，最后用隔离数据库验证真实学习工具。

## 包格式

原有仅策略的包保持兼容。完整包多出以下可选字段：

```json
{
  "id": "learning-agent",
  "version": "0.1.0",
  "api_version": 1,
  "entry": "loop.mjs",
  "config": { "review_after_record": true },
  "extension": {
    "command": "/absolute/path/smallhead-learning-worker",
    "args": [],
    "config_file": "binding.json",
    "tools_file": "tools.json",
    "system_file": "system.md",
    "timeout_ms": 30000
  }
}
```

三个数据文件必须位于包目录内，符号链接也检查最终位置。config 是本机配置对象，包含课程库路径、用户、课程等绑定，不由模型调用或 A2A 发送者指定。command 是 PATH 上的程序名或相对包目录的可执行文件路径；args 原样传递，不使用 shell。

tools.json 是工具数组，每项含 name、label、description、input_schema 和 access。schema 顶层须为 object；access 为 observe、mutate 或 execute。例如：

```json
[
  {
    "name": "learning_context",
    "label": "学习上下文",
    "description": "找回已绑定课程、章节、学习停点和观察",
    "input_schema": { "type": "object", "properties": {} },
    "access": "observe"
  }
]
```

工具名跨已加载包不得重复。与宿主已有工具重名时，本轮装配整体失败，不保留半份工具或提示词。失败重载保留旧版。加载不执行 worker；首次实际调用才启动。

## 运行链路

```text
/lens load 包目录
  → 校验并固定配置、提示词、schema、策略代码
  → 下一任务取得 LensSession 快照
  → attach(agent, host_access) 注册领域工具与提示词
  → 同一 AgentLoop 执行策略、模型和工具循环
  → 调用工具时启动 worker、绑定真实输入、执行 call
  → ToolResult 经过原有日志、历史、artifact 与投影
  → 结束/取消回收 worker
```

工具与循环策略来自同一份 session。直接 `/lens` 装卸不改变已接收任务；agent 主动通过 `lens` 工具装卸时，会为当前任务准备新环境，在这一批工具的结果全部配对之后、下一次模型调用前切换。其余正在运行的任务仍保留旧快照。新任务不再取得已卸载工具。worker 是本轮工具执行适配器，不是新的自治 agent loop，也不是需要用户维护的常驻学习服务。

## 当前任务内发现与装卸

2026-10-20 增加 `LensCatalog`、`ManagedLensSession` 与通用 `LoopEnvironmentSource`。宿主在任务接收时固定初始版本，调用 `attach_managed` 注册 `lens` 管理工具；core 不依赖包格式或学习业务，只在模型调用边界接收同一代 tools/system/policies。原始历史、工具凭据、输入来源与预算保持原样。

`discover` 搜索本地目录和登记包，返回 id/version/description/path/loaded/has_extension，不读出私有绑定。目录来自项目 `lenses/`、`.smallhead/lenses/`、用户 `~/.smallhead/lenses/`、项目 `.smallhead/lens-paths.json` 路径数组与 `SMALLHEAD_LENS_PATHS` 路径数组。扫描深度、目录数、结果数及元数据长度有界。

`load/reload/unload` 在发布前验证全套工具，成功后同时更新注册表与当前任务的待切换环境。未改变的包复用本任务中的策略和领域 worker，重载的包用新实例。冲突、读取失败和坏 manifest 不发布半成品。被替换的实例保留到任务结束，以便完成旧一批的 after_tool 和取消清理；结束时统一 shutdown/reap，重复清理仍保留回收句柄。

TUI 可按本地路径或发现的 id 激活。A2A 激活需要 mutate 和 execute，且路径必须属于本机目录发现结果或已有宿主登记；observe 仅能发现与查询。加载新策略会执行本机代码，不能仅根据其领域工具标为 observe 就向只读 peer 开放。

这不是包市场或自动编译服务；包与原生 worker 仍需事先放到本机。发现配置可持久化，激活状态目前只在本次宿主运行保留。

## Worker 协议

stdin/stdout 每行一个 JSON，对应相同数值 id。请求 `{id, method, params}`；响应 `{id, result}` 或 `{id, error}`。

- initialize：`{api_version:1, config:...}`，成功返回 `{api_version:1}`。
- bind_input：`{input:{id,text,source}}`，来源为 user/peer/delegate；来自 ToolContext 的宿主记录，不接受模型参数替代。
- call：`{name,input,tool_call_id}`，返回标准 ToolResult，包括原始 tool_call_id、is_error 和 content。

只在接受的输入 id 变化时重新绑定。连续多个 steering 当前只绑定最后一条已消费输入；此前文字仍在对话历史，不能自动当作本次逐字记忆依据。未来可扩展为完整输入账本。

每次工具调用整体受 timeout_ms 和任务取消控制，包括启动及初始化。帧上限 16 MiB。协议错位、超时或取消后实例作废，后续调用不重启、不重放未知副作用。语义错误用正常 ToolResult.is_error 表达，允许模型按实际错误修正参数。stderr 只保留有界尾部用于错误诊断，不直接打印进 TUI。

## 宿主边界

TUI 在本地启用已加载工具。A2A 按本轮权限过滤工具：observe 只读，mutate 写领域状态，execute 执行命令。混合读写的工具保守标为 mutate，因此只读 A2A 可通过 learning_context 读取 checkpoint，但不能调用兼有 update 的 learning_checkpoint。包的注解是可信本机扩展的声明，不是 OS 沙箱；worker 本身仍有当前用户的系统权限。

学习 worker 不把 peer/delegate 来信当作学习者原话保存。可按授权查询学习资料或维护有依据的状态，但不能伪造学习表现。是否为人类输入由宿主入口决定，不从文本中猜测。

配置、schema、提示词和策略入口在 load/reload 固定。外部可执行文件及其自行读取的文件内容不受这个代码快照保证约束；更新原生 worker 前应结束旧任务。子 Monad 不自动继承父任务的领域工具，仍需显式提供能力。

此版不新增包市场、自动编译任意 Rust、插件专用 TUI 页面或自动方法采用。学习包保存在 Learning Agent 项目内，Smallhead 只提供通用运行契约。
