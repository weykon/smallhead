# Monad demo 第五段：自动业务派发

运行：

```sh
cargo run --example monad_demo -p smallhead-core
# 离线协议验证
cargo run --example monad_demo -p smallhead-core -- --mock
```

第五段主模型只获得 `monad_bridge` 工具。模型先调用 discover，获取业务 ID、说明及输入 schema；再调用 dispatch，提供业务 ID、目标和业务输入。桥接器从 StaticMonadRegistry 创建实例，构造真实 handoff，调用 start/step，校验结果关联，并通过原有 tool_call_id 将结构化结果送回主模型。

终端依次显示 `[parent]`、`[discover]`、`[dispatch]`、`[return]` 和 `[parent result]`。两个子任务使用不同 task_id 和 result_slot，共享程序绑定的父 task_id。模型不生成关联 ID。

真实模式下业务选择与参数来自模型；Mock 模式下这些决策来自预设响应，但子 Monad 的计算和回填仍实际执行。不要把 Mock 的最终文本作为模型正确率证据。

范围：单层串行桥接，子业务是确定性 Rust 实现。主模型是有工具循环的 Agent，子业务尚无独立 LLM 循环。未接 Laya/Qwen、持久化任务图、恢复或预算强制执行；回填通过工具结果进入主上下文，尚无独立 result_slot 存储。WaitingContext 会原样返回供主模型处理，本版不实现原子任务恢复。目录随 discover 返回，不是最初全部塞进系统提示。

实现：`src/monad_bridge.rs`，`ToolKind::MonadBridge` 和 `examples/monad_demo.rs`。桥接器目前限于三个内置 fixture；它的输入 schema 与 fixture 要一起维护。不要将其误当作支持任意插件的生产调度器。

验证覆盖真实 AgentLoop + MockProvider 的发现、两次派发、计算结果、父子关联、独立任务 ID、未知业务、非法参数和 WaitingContext。顺带修复 AgentLoop 对中文工具结果预览按字节切片可能 panic 的问题。

## 按业务配置模型偏好（2026-10-08）

第五段支持 `--prefer-fast=laya` 或 `--prefer-fast=qwen`，只为 calculation 显式配置快速算子选择；echo、greeting 和所有未配置项默认 `LargeModel`。

```sh
# 主模型真实 API + 本机快速服务
cargo run --example monad_demo -p smallhead-core -- --prefer-fast=laya
cargo run --example monad_demo -p smallhead-core -- --prefer-fast=qwen

# 主模型脚本响应 + 本机真实快速服务；--mock 不会 mock Laya/Qwen
cargo run --example monad_demo -p smallhead-core -- --mock --prefer-fast=qwen
```

服务未运行时，分别执行 `sh experiments/laya/start.sh` 或 `sh experiments/qwen-reranker/start.sh`。默认端口 3031/3032，可用 SMALLHEAD_LAYA_URL / SMALLHEAD_QWEN_URL 覆盖。

配置在 `MonadBridgeTool.model_preferences`，由应用绑定，不由模型工具参数覆盖。`ModelPreference` 是带 serde tag 的 enum：缺省 LargeModel；PreferFast 携带 backend、endpoint、timeout_ms。discover 将配置告知主模型，进入业务后的路由由程序按配置执行。

当前边界：主模型已经拆解任务并提取 expression；快速模型只选择 add/subtract/multiply/divide/defer。接受后用所选方法和原始操作数构造执行表达式，再交给 CalculationMonad。它不是快速模型直接理解任意自然语言、抽取参数或独立 Agent 推理的实现。由于 expression 已含算子，这里是路由机制实验，实际算术可以直接用解析器；此版本增加本地调用，不宣称节省总延迟或大模型调用。

`[model-route]` 和工具结果记录实际后端、选择、原始分数、耗时与回退原因。Laya 分布与 Qwen 独立分数分别校验，不共用置信度阈值。算术选择额外与已提取表达式的算子比较；不一致、defer、无效响应、HTTP 故障或 5 秒超时，沿用主模型已经提供的表达式。这里的“回退”是复用已有主模型决策，不是新发一次大模型请求。其他业务没有这种确定性校验前不能照搬接受策略。

2026-10-08 本机真实快速服务 + Mock 主模型验证：Laya 加法选成乘法而触发回退（约137ms），乘法通过（约28ms）；Qwen 两项通过（约1089ms、347ms）。两条链都实际执行并返回200、10000。只是本次样本，不是准确率或稳定性能结论。默认路径、响应校验、算子不一致、HTTP 不可用均有回归测试。

## 主 demo 入口（默认已接入 Monad）

```sh
cargo run --example demo -p smallhead-core -- "帮我计算12加7，再计算9乘以8"
cargo run --example demo -p smallhead-core -- --prefer-fast=qwen "计算100乘以100"
cargo run --example demo -p smallhead-core -- --direct-tools "计算100乘以100"
cargo run --example demo -p smallhead-core -- --mock --no-save
```

默认只开放业务桥接工具，主请求经 MainMonad 包装，保留 discover/dispatch/return/parent result 日志。`--direct-tools` 回到旧版10个工具路径，不能同时使用 `--prefer-fast`。恢复会话仍加载历史消息，每次新请求有新的父任务ID；模型偏好每次由 CLI 指定，不从历史自动恢复。主循环 Completed 表示本轮回复结束，未额外证明所有用户目标都完成；需结合子结果与回复检查。

Mock 为固定的加法和乘法脚本，不理解自定义输入；真实模式才由模型决定业务和参数。`--mock --prefer-fast=qwen` 仍访问真实本机 Qwen。`--no-save` 同时禁止初始化与完成后的会话写入。
