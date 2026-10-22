# 可组合 Loop 策略：抽象与参考

2026-10-19。核心策略链、四个决策点、进程外 JS 和 TUI 加载入口已落地；实际 API、使用方式与边界见 [smallhead-lens](../crates/smallhead-lens/README.md)。下面保留设计推导，StatePatch、通用效果解释器等仍是后续方向，不代表已经实现。工程文档按当日在线内容核对，不将访问时间当作发布日期；理论论文保留原始年份。

## 建议的核心抽象

将执行机制、运行策略和策略变换分开：

```text
运行策略 Policy：根据状态与新事件，提出下一步
Lens：接入一个 Policy，产生一个新的 Policy
执行机制 Driver：落实下一步，把真实结果作为新事件送回来
```

示意类型：

```text
Policy     : Snapshot × Event → Transition
Transition : StatePatch × Action
Lens       : Policy → Policy
```

Snapshot 包含必要的历史/证据引用、当前阶段、能力目录、预算观察和该策略可见的状态。Event 可以是用户输入、steering、模型响应、工具结果、子 Monad 报告或用户答复。

Action 是有类型的下一步请求，例如 RequestModel、ExecuteToolBatch、Delegate、AwaitUser、Finish、Fail。具体变体先以现有 loop 能支持的行为为准，不把未来接口当作已实现能力。

策略提出 Action，不直接执行它。Driver 校验阶段与身份、执行请求、保存回执，再把结果送回策略。完整工具调用配对是宿主协议；内部策略事件不是每条都要伪装成用户消息或塞进模型上下文。

system、模型可见工具、消息投影与模型参数是 RequestModel 的结构化内容，Lens 可按状态变换它们。工具语义适配必须把模型可见 schema、实际执行入口和回执关联起来，不只替换工具名称。

模型自主选择和代码控制在此共存：模型在给定 system/上下文/工具中提出行动，Lens 决定如何准备和处理本轮交互，Driver 保证实际执行及其记录。

## 两层组合，不用一个万能事件接口

| 层次 | 要解决的问题 | 示例 |
| --- | --- | --- |
| 策略组合 | 接到事件后下一步做什么 | 把准备结束的回答转为一次有依据的补充核对 |
| 效果包裹 | 已决定的模型或工具调用怎样执行 | 调整模型请求、适配工具参数、记录真实调用耗时 |

二者可由同一 Lens 包提供，但调用契约不同。完成判断不能只靠一个被忽略返回值的通知回调，工具执行也不能被“多监听一次事件”隐式重复。

如果策略需要先查询数据库才能决定，就提出受支持的读取效果，等结果事件返回后再决定；JS SDK 可以提供 async 语法隐藏这种分步。原始 Node IO 不是自动受宿主控制的效果，不能把 worker 进程分离当作完整效果追踪或沙箱。

对于最小实现，优先提取 RequestModel 的构造和 Finish 候选的处理，随后再扩展工具与委派策略；无需一次引入通用解释器或把已有 loop 全部重写。

## 组合的数学含义

给定基本策略 P，A、B 都把策略变成策略：

```text
最终策略 = A(B(P))
(A ∘ B)(P) = A(B(P))
I(P) = P
```

这里 A 是外层。调用进入时先经过 A 再到 B，返回决定时 B 的结果先回到 A；这让外层能够在内层判断的基础上再次处理，而不是多个插件各自运行一套主循环。

在策略变换的构造层，目标是保持函数组合的结合律和恒等变换：`(A ∘ B) ∘ C = A ∘ (B ∘ C)`、`I ∘ A = A ∘ I = A`。组合构造不执行安装或其他业务副作用，实例状态与次序必须明确，才能检验这类性质。

组合通常不交换：`A ∘ B` 不等于 `B ∘ A`。例如先选择相关证据再压缩，与先压缩全部材料再选择，可能取得不同的模型上下文。因此顺序是配置和诊断的一部分。

这些是接口组合性质，不意味着带随机模型、时钟、网络的两次真实运行会产生完全相同答案，也不自动证明两个策略业务上兼容。

产品里的 Monad 是执行实例，Lens 是运行策略变换；不能仅凭名称认定它们已经满足 Haskell Monad 法则或经典双向 Lens 的 get/put 法则。当前最贴切的理论参照是中间件与效果处理。

## JS 作者看到的形态

下面是伪代码，只说明组合语义，不是当前 SDK：

```js
export async function wrapStep(snapshot, event, next) {
  const decision = await next(snapshot, event);
  if (decision.action.type !== 'finish') return decision;
  if (!needsSourceCheck(snapshot, decision)) return decision;

  return {
    ...decision,
    action: {
      type: 'request_model',
      request: buildSourceCheckRequest(snapshot, decision),
    },
  };
}
```

此处 next 只计算内部下一步，不已经执行模型、工具或提交最终回答，所以替换 Finish 不会倒退已经发生的副作用。decision 的状态补丁保留；需进一步修改时走声明过的合并规则。

needSourceCheck 的判断要来自实际任务要求和证据状态；来源缺失、确认不可取得或预算不足必须有退出路径。不要把模型一句“查过了”当作真实工具回执，也不要遇到缺失就无条件继续。

进程外不能传递 JS/Rust 函数指针。SDK 的 next 需要通过宿主持有的 continuation id 或分阶段协议实现，并绑定实例代次、阶段和取消令牌；这不提供跨进程重启后的调用栈恢复。

## 必须写进契约的组合规则

| 对象 | 推荐规则 |
| --- | --- |
| 通知 | 仅观察，不接受控制流返回值；明确是否等待，避免阻塞流式响应 |
| 请求变换 | 显式顺序，下一层看到上一层输出；保留未声明修改的字段 |
| 策略 next | 首版每次调用最多一次；返回后的决定可再变换，不自动重复执行 |
| 效果 next | 默认每个调用身份最多执行一次；额外尝试必须走显式重试并记录新 attempt |
| 控制流 | 逐层处理一个有类型 Action；多个独立主控制器并列时拒绝或显式选择，不随机投票 |
| 状态写入 | 每个 Lens 有命名空间；共享字段有声明的 owner 或 reducer，冲突不静默最后写入覆盖 |
| 结束与继续 | 候选回答可被处理为继续；记录哪一层、依据什么证据改变了收尾决定 |
| 原始事实 | 原始工具回执保留，模型可见观察可以变换，不能冒充新的执行事实 |

next 最多一次是 Smallhead 的首版工程选择，不是所有 middleware 或代数效应系统的通用定律。多次恢复 continuation 可以表达搜索和重试，但对已执行工具尤其容易重复副作用；先不默认开放。

只读引用式 Snapshot 加返回的 StatePatch，可以减少插件直接共享可变 history 的混乱。准备模型请求时可组合带 id/owner 的 system 段、证据引用和工具视野，再由宿主生成 ProviderRequest；不要求所有 system 变更都是简单字符串 append。

## 与当前 Rust 代码对齐

目前 `crates/smallhead-core/src/agent_loop.rs` 的 run_turn_inner 同时负责请求构造、模型等待、工具执行和收尾判断。可以先抽出 DefaultLoopPolicy，保持未启用 Lens 时的行为。

core 提供策略契约、阶段数据、指令及默认实现。Lens crate 负责脚本适配、组合与版本；TUI/A2A 共用宿主装配。Provider、工具执行、steering、压缩、Monad 协作与清理继续复用，领域 Lens 不复制它们。

Action 发起前仍检查取消和预算，返回后提交真实事件。插件提出新一步不等于该步已执行。已执行工具不能因策略改向而回滚或重新标为未执行。

## 参考资料与可借鉴内容

### 1. LangChain Agent Middleware：最接近这次需求

- [Agent Middleware，2025-09-08](https://www.langchain.com/blog/agent-middleware)
- [Custom middleware，当前官方文档](https://docs.langchain.com/oss/python/langchain/middleware/custom)

文章直接讨论如何改动 agent 的状态、模型输入与步骤次序；官方接口区分节点式 hook 和包裹模型/工具调用的 middleware，并规定进入与返回的顺序。

对我们最有用的是“包裹一次计算”的接口及明确的组合次序，而不只是在 loop 周围发通知。其库允许某些 handler 多次调用；Smallhead 对副作用调用应采用自己的明确重试约定。阅读重点：Hooks、Execution order、Agent jumps。

### 2. Pi Extensions：同一循环中的不同组合契约

- [当前官方 Extensions 文档](https://pi.dev/docs/latest/extensions)
- [官方扩展类型定义](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/extensions/types.ts)

文档分别定义通知、链式变换、操作拦截和可继续的边界。context 和 tool_result 等会传递前一层的变换；turn_end/agent_before_settle 可以请求下一次模型运行。

它为 Smallhead 提供一个直接教训：每种扩展点需要明确返回值是否生效、如何与前一层合并。不能只设计一个 event emitter，再期待所有回调自然组成流程。阅读重点：Follow the extension contracts、Understand handler composition。

### 3. Tower Layer / Service：Rust 里的组合骨架

- [Tower Layer 官方接口](https://docs.rs/tower/latest/tower/trait.Layer.html)

Layer 接收内部 service 并返回包裹后的 service。它是“运行策略变换”在 Rust 中很接近的接口参考。

可以借鉴类型分层、装配和调用包装，但 agent 还有多阶段状态、证据及等待用户，不能直接把整个系统当作一次 HTTP 请求。是否实际使用 Tower crate，应由原型复杂度决定，不必为一个抽象类比引入依赖。

### 4. 代数效应与 Handler：解释为什么需要分离下一步与执行

- [Pretnar，An Introduction to Algebraic Effects and Handlers，2015](https://www.eff-lang.org/handlers-tutorial.pdf)
- [Plotkin、Pretnar，Handling Algebraic Effects，2013](https://arxiv.org/abs/1312.1399)

这组研究把程序提出的操作及其处理分开，并用 continuation 表达操作完成后如何接续。教程比正式理论更适合作为第一篇阅读。

对 Smallhead 的迁移是：把模型调用、工具、等待等做成有类型操作，Lens 可以处理/变换它们，Rust 统一执行。这是从论文提炼的设计建议，不是论文已经研究了 LLM Lens，也不代表 Rust/JS 原生提供了完整的代数效应系统。先读教程前几节的操作与 handler 例子，再读正式语义。

### 5. ReAct 与 DSPy：分别说明底层循环和上层可编程性

- [ReAct，2022 预印本、ICLR 2023](https://arxiv.org/abs/2210.03629)
- [DSPy，2023 预印本](https://arxiv.org/abs/2310.03714)

ReAct 说明推理与行动交替的基本方式；它本身没有解决多个运行策略如何装卸与组合。DSPy 则把模型调用表达为可组合模块，并研究如何依据指标优化程序。

DSPy 对 Learning Lens 后续评估和策略改进有启发，但“能优化一个任务指标”不能直接解释为能自动优化真实学习效果；需要独立学习证据和评测设计。两者可作为背景，首轮接口设计先读前四组。

## 首轮原型的验收

1. 空 Lens 与默认策略生成相同指令；不新增模型调用。
2. 两个请求变换按明确顺序叠加，日志能还原实际 system、上下文来源和工具视野。
3. 相同候选在未取得所需证据时被一个 Lens 转为继续，取得证据后可以结束；来源不可得也能退出。
4. 外层 Lens 能基于内层决定继续处理，测试默认透传、短路及错误归因。
5. 请求 next 两次被明确拒绝；不因插件重试自动重复有副作用的工具。
6. 中途 steering、取消、预算耗尽和 worker 崩溃时，保持工具协议和资源清理；不悄悄移除策略后宣称成功。

最先值得实现的是 DefaultLoopPolicy、显式 Action、可组合的请求变换和 Finish 判断。真实 JS worker 是随后接入这套契约的适配，不能让 IPC 细节反过来决定 agent 的抽象。
