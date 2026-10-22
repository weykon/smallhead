# Smallhead Lens

Lens 是可组合的 agent loop 策略。默认仍是原来的 ReAct；加载的 Lens 用代码包裹每个决策点，不复制 AgentLoop。

```text
learning(evidence-check(default ReAct))
    ↓ prepare_model → provider → before_tool → tool → after_tool
    ↑                 before_finish → finish / continue
```

## 直接使用

需要 Node.js 20+。在仓库目录启动 TUI 后输入：

```text
/lens load lenses/learning
/lens load lenses/evidence-check
/lens list
```

先加载的是外层。试问：“找一份关于 Rust 所有权的官方资料，解释给我，再给我一道小练习。”这里 `lenses/learning` 仍是纯策略演示；完整 Learning Agent 包另外提供 Rust 学习 worker、课程和记忆工具，不应混淆。Evidence Check 会在第一次候选结束时追加一次证据核对，不证明核对后的结论一定正确。

完整 Learning Agent 原生宿主和领域 worker 位于独立项目，未随此仓库发布；此仓库提供通用 Lens 接口和示例策略。

```text
/lens reload learning
/lens unload evidence-check
/filter lens
```

`/lens` 命令影响下一次接收的用户任务，其他已接收任务继续使用固定版本。agent 也能通过 `lens` 工具主动发现、加载、重载和卸载；这些显式操作会在当前任务的下一次模型调用前切换提示词、工具与策略，同时更新宿主注册表供未来任务使用。当前一批工具先完成结果配对，新增工具不能在同一批提前调用。对话、真实用户输入、预算与任务身份不重建；未改变的 Lens 保留私有状态。TUI 注册表目前仅在本次运行保留。

## Agent 自行选择 Lens

自然地说“找一个合适的学习 Lens，找回学习停点，再给我一点提示”。agent 可调用：

```text
lens(action=discover, query="learning")
lens(action=load, id="learning-agent")
  → 下一次模型调用已含学习工具，继续原任务
learning_context(...)
```

`list` 返回本任务选择的层和宿主登记；`discover` 搜索本机可用包。发现不执行代码，也不返回 config 或 binding。manifest 可增加可选 `description`，说明用途，避免把纯策略示例和完整领域包混为一谈。

默认搜索工作目录的 `lenses/`、`.smallhead/lenses/` 和 `~/.smallhead/lenses/`，向下最多三层。可用 `.smallhead/lens-paths.json`（JSON 路径数组，路径相对工作目录）或 `SMALLHEAD_LENS_PATHS`（同样格式）登记其他包目录或搜索根。例如：

```json
["../agenting-agent/learning-agent/lenses/learning"]
```

目录配置跨重启保留可发现性，不会自动激活全部包。读取失败会明确报错。TUI 还允许 agent 按用户提供的本地路径加载。加载前会验证完整候选环境；名称冲突、坏配置或重载失败时，注册表与当前工具保持原样。worker 仍按需启动。卸载后移除可调用能力，已启动的旧 worker 保留到本任务结束统一回收，领域数据库不删除。

无 API、无付费模型的完整示例：

```sh
cargo run -p smallhead-lens --example lens_demo
```

A2A 服务端可由本地启动环境指定包目录，走相同的策略接口：

```sh
SMALLHEAD_LENSES='["/absolute/path/lenses/learning","/absolute/path/lenses/evidence-check"]' cargo run -p smallhead-server -- serve
```

`SMALLHEAD_LENSES` 指定 A2A 启动时直接加载的包；TUI 不读取这个变量。`SMALLHEAD_LENS_PATHS` 仅登记发现目录，两种宿主都支持。

完整学习包可再叠加仓库中的 `teaching-feedback`，按顺序形成 `learning-agent(teaching-feedback(ReAct))`。已加载完整学习包时输入 `/lens load lenses/teaching-feedback`，或让 agent 发现并加载 id `teaching-feedback`。本层根据真实学习回执指导反馈修正，不添加模型或强制收尾轮；未注册学习工具时透传。加载、重载与卸载见 [包说明](../../lenses/teaching-feedback/README.md)。

A2A 只读请求仅开放 `discover/list`；加载、重载、卸载同时需要 mutate 与 execute 权限。A2A 只能加载本地目录配置或宿主已有登记中发现的包，不允许用任意远端路径激活代码。工具本身执行时再次检查权限。子 Monad 不自动继承父任务的 Lens 管理或领域工具。

A2A 真实模型需先导出 `MINIMAX_API_KEY`。要通过 A2A 修改代码和运行测试，在上述 `serve` 后加 `--access development --cwd 项目路径`，发送端使用 `send ... --development`。默认请求仍为只读；详见 [A2A 协议与开发入口](../../../docs/a2a-protocol.md)。

## 写一个包

```text
my-lens/
  lens.json
  loop.mjs
```

```json
{
  "id": "my-lens",
  "version": "0.1.0",
  "api_version": 1,
  "entry": "loop.mjs",
  "config": { "style": "concise" }
}
```

```js
let reviewed = false;

export async function apply(context, event, next) {
  if (event.type === 'prepare_model') {
    event.request.system += '\nExplain concretely and check the actual evidence.';
  }
  const action = await next(event);
  if (action.type === 'finish' && !reviewed && context.can_continue) {
    reviewed = true;
    return { type: 'continue', guidance: 'Review this candidate once, then answer.' };
  }
  return action;
}
```

`next(event)` 只计算内层决定，还没有执行模型或工具；每次 checkpoint 最多调用一次，用了就要 await。Rust 的 next 是不可克隆、按值消耗的 continuation，由所有权在编译期阻止重复调用；JS 在协议层检查同一约束。返回时外层可以进一步改写 action。也可直接返回当前阶段允许的 action。内层错误不能被静默吞掉。

JS 的模块状态与 `context.config` 属于本轮实例；每个新任务会得到独立 worker。`context` 另有 `turn_id`、`wave`、`tool_calls`、`can_continue`、`finalization_reason`。V1 的入口是自包含 ES module，支持 Node 内置模块；不支持相对导入及第三方包解析。入口代码和配置在 load/reload 时读取固定，外部文件和原生 Node IO 不受这个版本固定保证约束。

## 决策契约

| Event | 默认 Action | 可改变的行为 |
| --- | --- | --- |
| `prepare_model {request}` | `model_request {request}` | system、上下文文本投影、工具子集/描述/label、预算内的输出 token 和 temperature |
| `before_tool {call}` | `execute_tool {call}` | 保留 ID/名称，调整参数；或 `reject_tool {reason}` |
| `after_tool {call,result}` | `observe_tool {result}` | 保留 ID/is_error，调整模型观察；私有状态可依真实回执更新 |
| `before_finish {candidate}` | `finish` | 或 `continue {guidance}`，受本轮模型预算约束 |

工具 schema 与身份属于宿主注册表，不能靠改描述凭空增加工具。隐藏的工具即使模型提出调用也不会执行。参数仍由工具自己的输入校验处理。请求投影不能删改已有调用/结果的配对协议；源历史不会被投影覆盖。

Rust 内置 Lens 实现 `smallhead_core::loop_policy::LoopPolicy`，用 `PolicyStack::new(vec![outer, inner])` 和 `AgentLoop::set_policy_stack` 接入。自定义 host 用 `registry.session()` 为每轮创建实例，持有 session 至该轮结束或取消，再 `shutdown().await`。同一个 JS session 不允许跨 `turn_id` 复用。

委派、用户选择、暂停、等待仍通过已有的宿主工具与 Monad 通道完成；当前没有额外的 `delegate`/`await_user` 策略 Action。TUI 给主 Monad 挂载加载的链，子 Monad 默认使用自身策略，不自动继承父 Lens 或权限。

## 记录与生命周期

- 每个非空策略 checkpoint 的阶段、顺序、耗时、决定和错误进入 `lens` 诊断日志；完整的最终模型请求仍可在 request 日志查看。
- 工具真实回执在调用 `after_tool` 前提交。`lens_tool_receipt` 内部通知持久化实际参数与未被 Lens 改写的观察；大内容沿用 artifact 引用。修改后的观察另作为 ToolResult 进入模型历史，不能改变真实成功/失败状态。
- 候选回答可先流式显示，但只有策略接受收尾后才产生正常 TurnEnd。继续指导明确标为 Lens 生成，不当作用户新授权。
- 每个 checkpoint 整条链最多 10 秒；取消立即打断。协议单帧上限 16 MiB；改写观察再次经过大文本投影，改写请求按现有 bytes/4 估算检查模型窗口。这不是精确 tokenizer。
- 错误会终止本轮并保留回执，不静默撤掉 Lens 继续声称成功。协议损坏、调用取消后 worker 作废；不会重放未知副作用。
- 正常结束显式终止并回收 worker，丢弃在途 future 时用 kill-on-drop 收尾。`console.*` 走 stderr，stdout 专供协议；错误可附带最多 4 KiB stderr 尾部。

Node 插件拥有当前用户的 OS 权限，进程隔离不是安全沙箱。原生 Node IO 不受宿主效果追踪；插件自行启动的后代进程也不保证随 worker 一起回收。

## 包内领域工具

可选 `extension` 声明外部工具 worker、本机配置、工具清单与领域提示词。TUI 加载包后下一轮自动注册；A2A 按宿主权限过滤；卸载撤掉新任务中的注册，数据由扩展自己的存储保留。工具、策略和提示词来自同一份已加载快照，失败重载保留旧版。格式、协议、来源与生命周期见 [领域扩展契约](../../docs/lens-domain-extensions.md)。

自定义宿主在设置策略时另调用 `session.attach(&mut agent, ExtensionAccess::all())?`。工具只在调用时启动 worker，正常结束或取消后随 session 清理；无需把学习业务编译进 TUI 或 core。Rust 工具需要预先构建 worker，加载 Lens 不自动编译源码。当前没有动态 TUI 视图或通用包安装器。
