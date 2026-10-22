# Laya / Qwen 快速决策能力与使用

更新：2026-10-09。Rust 客户端、Agent 工具和 TUI 已接通。协议测试使用本地模拟服务；随后已启动本机 3031/3032 服务并实际跑通两个 demo，结果见下文。这不等于完成准确率或性能评估。

## 能力范围

| 后端 | 能力 | 结果语义 | 适合的业务 |
|---|---|---|---|
| Laya | choice | 一个标签与完整候选概率分布 | 工具、业务、数据源单选 |
| Laya | score | 有序等级上的数值、等级定义与概率分布；可能是小数 | 紧急程度、相关程度等分级 |
| Laya | noul | 原生 0..1 是非值，不强制转成 bool | 判断用户是否表达某种需求 |
| Laya | 多问题 | 一次 `/evaluate`，共享 state，按问题 ID 返回不同类型答案 | 同时判断来源、等级、语义条件 |
| Qwen | rank_all | 全部候选排序，默认策略 | 把排序结果交给主模型 |
| Qwen | top_k | 前 K 项，可显式指定 min_score | 选择多条记忆、资料 |
| Qwen | filter | 所有达到 min_score 的候选，可以为空 | 资料筛选 |
| Qwen | select_one | 最高项同时达到 min_score / min_margin 才接受；并列最高拒绝 | 工具或业务单选，歧义时 defer |

Qwen 每条候选的 yes/no 相关性分数独立，不要求总和为 1。Laya choice/score 的概率分布需要加和约为 1。两个后端的分数不能直接比较，也不能共享未经校准的阈值。Laya confidence 和 noul 值不证明业务事实，更不能证明权限、执行成功或任务终态。

这是当前固定本地服务协议支持的能力集，不代表已经接入上游所有部署后端、模型 Router、校准训练或结构化框架集成。Laya 的多问题不会在 Rust 里额外拆成多个 HTTP 请求；模型后端是否共享计算、速度如何，应另外实测。

## 亲手体验，不用聊天模型 API

先分别启动需要的本地服务：

```sh
sh experiments/laya/start.sh
sh experiments/qwen-reranker/start.sh
```

每个服务占一个终端；已运行时不要重复启动。另开终端：

```sh
# 只查看代码支持的能力，不访问服务
cargo run -p smallhead-core --example fast_decision_demo -- catalog

# 同时问单选、等级、是非三个问题
cargo run -p smallhead-core --example fast_decision_demo -- laya

# 同一组记忆候选，体验不同策略
cargo run -p smallhead-core --example fast_decision_demo -- qwen rank_all
cargo run -p smallhead-core --example fast_decision_demo -- qwen top_k
cargo run -p smallhead-core --example fast_decision_demo -- qwen filter
cargo run -p smallhead-core --example fast_decision_demo -- qwen select_one
```

每条命令显示具体请求、规范化输出和原始响应。filter/select_one 中的 0.5 / 0.1 是观察策略行为的示例参数，没有经过业务校准，不能当作生产默认值。客户端默认 5 秒超时，服务地址读取 `SMALLHEAD_LAYA_URL` / `SMALLHEAD_QWEN_URL`，缺省为本机 3031/3032。

## 本机实际体验记录

2026-10-09：最初两个服务均未启动，客户端连接失败；启动脚本成功加载现有权重后，两条 demo 均成功返回。demo/TUI 不自动启动 Python 模型服务，需要保持对应服务进程运行。

- Laya 多问题请求：推理约 269.42 ms，source 选 orders；priority 得分 1.6492/2，对“不急”判断偏高；history 原生值 0.0623，对“昨天”的判断也需要进一步核对与评估。不能只看 confidence 高就视为判断正确。
- Qwen 三候选 top_k：推理约 2240.07 ms，语言记忆评分约 0.9966，另外两项约 0.0159 / 0.0096。top_k=2 且没有最低分，仍保留第二项，这是指定策略的行为；只想接受相关内容时需显式增加经过评估的最低分要求。

以上均为单次体验数据，不是稳定延迟、并发性能或生产准确率。

## 在 TUI 中由 Agent 使用

```sh
cargo run -p smallhead-tui
```

输入 `/route auto`，再发送：

> 请 discover fast_decision 的能力。用 Laya 同时判断“请用中文告诉我昨天订单的状态，不急”的数据来源、紧急等级和是否涉及历史。只解释决策结果，不实际查询订单。

或者：

> 使用 Qwen，把“用户希望中文回复”“项目使用 Rust”“用户喜欢简短解释”按与“回复语言偏好”的相关程度排序，保留前两项。请真实调用 fast_decision，不直接猜测结果。

`/route auto` 允许两个后端，`/route laya` / `/route qwen` 只允许对应后端，`/route large` 可发现目录但不调用快速服务。切换影响下一轮请求。Agent 可以选择不调用；用户明确要求体验调用时应真实调用，服务失败则报告失败，不能假装已取得模型结果。`--mock` 的 TUI 仍是固定计算脚本，不理解上述自定义请求。

`monad_bridge` 负责业务发现与执行，`fast_decision` 负责建议与排序；计算 Monad 原有的快速单选路径继续保留。新工具不会自动把选出的候选变成工具调用，也不会修改 TaskSlot 状态或读写记忆库。

如果请求是「用快模型计算 100×100，再计算 100-10」，应分别调用 `monad_bridge.dispatch`，提供表达式 `100*100`、`100-10`，在 auto 模式下明确填写允许的 `model_choice=laya|qwen`。bridge 内部调用快速模型做方法选择，本地 calculation 求值；不必先额外调用 fast_decision 选择同一方法。快模型不是计算器，不意味着不能参与计算业务。只有 discover 日志表示读取目录；必须查看后续 model-route 才知道实际调用、采用或回退。

在 `/filter route` 中查看：

- `decision_request`：具体局部问题和候选。
- `decision_response`：HTTP 耗时、原始响应或服务错误。
- `decision_complete`：经过解析校验及策略处理的最终建议。

没有 complete 但有 response 时可能是输出校验失败，查看 tool 的错误结果。它会送回主模型继续决策；不自动伪造回退分数。日志含提供的局部数据，沿用 TUI 的保留与导出机制。

## 开发接口和走读顺序

`crates/smallhead-core/src/fast_decision/`：

1. `protocol.rs`：LayaQuestion 枚举与两种请求、Candidate、RankingPolicy。不同语义用类型区分；未知字段拒绝。
2. `mod.rs`：DecisionClient，应用配置地址、超时、HTTP 请求及诊断事件。非 Agent 调用者也能直接使用 evaluate/rerank。
3. `laya.rs`：问题与局部上下文验证，生成问题身份与 SHA-256，校验回包身份，按类型保留答案。
4. `qwen.rs`：候选索引转稳定 ID，检查重复、缺项和非法分数；解析完成后独立应用业务策略。并列排序按原候选顺序稳定输出，单选不会凭顺序接受并列结果。
5. `tool.rs`：fast_decision 工具，action=discover/laya/qwen；能力目录、输入 schema 与允许后端检查。Agent 不提供 endpoint。
6. `tests.rs`：模拟 HTTP + AgentLoop 全链路、类型解析、排序与拒绝、输入错误、后端限制和服务故障。

最小调用契约：

```json
{"action":"laya","request":{"state":{"query":"请查昨天订单"},"questions":{"source":{"type":"choice","instructions":"选择数据源","criteria":{"orders":"订单库","memory":"用户记忆"}}}}}
```

```json
{"action":"qwen","request":{"query":"回复语言","instruction":"排序相关记忆","candidates":[{"id":"m1","text":"用户喜欢中文"},{"id":"m2","text":"项目使用 Rust"}],"policy":{"mode":"top_k","k":1}}}
```

Laya 返回 status=advisory 与按 ID 组织的 answers。Qwen 返回完整 ranking，以及策略选出的 selected；没有候选通过则 status=defer。两者均 executed=false，并保留 raw_response。输出格式不伪造共同“正确置信度”。

当前尚未接通任务 revision / 候选集合 version，因此建议供本次调用方使用，不能直接当作跨任务版本的授权。后续接入真实记忆与工具目录时，由业务层构造经过权限、有效期过滤的候选，选择策略和校准集也由业务定义。
