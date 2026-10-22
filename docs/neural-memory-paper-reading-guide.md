# 神经记忆与可学习记忆：论文阅读路线

整理日期：2026-10-16。目标是理解数学、实验和工程假设，尚未选定 smallhead 的记忆架构。

这里补充此前的 world-memory 研究记录；此前讨论过的数据库、摘要、索引方案并不因此成为已批准设计。

## 1. 先区分三个学习对象

| 路线 | 什么在变化 | 典型论文 | 关键问题 |
| --- | --- | --- | --- |
| 神经关联记忆 | 矩阵、隐状态、网络快权重 | Fast Weights、DeltaNet、TTT、Titans | 如何把历史压入有限状态，并可靠读出？ |
| 可学习的记忆管理 | 写入、修改、删除、读取的策略 | Memory-R1、Mem-alpha、AtomMem、UMA | 哪些操作有助于未来任务？ |
| 经验效用学习 | 外部经验的效用估计和选择策略 | MemRL | 语义相似的经验，是否真的有用？ |

“系统有学习”不一定意味着记忆内容保存在神经网络权重里。三条路线可以组合，但论文中的训练对象和证明对象不能混用。

## 2. 论文地图

年份以首次公开时间为主，会议年或修订时间另标。日期来自原始论文页面，不能把搜索引擎的抓取时间当发表时间。

| 论文与原文 | 时间 | 阅读时抓住的问题 | 核验深度 |
| --- | --- | --- | --- |
| [Neural Turing Machines](https://arxiv.org/abs/1410.5401) | 2014 | 如何让读写地址可微，从任务损失学出操作？ | 原始摘要 |
| [Using Fast Weights to Attend to the Recent Past](https://arxiv.org/abs/1610.06258) | 2016 | 权重能否充当临时记忆，而非只存训练知识？ | 原始摘要、会议来源 |
| [Hybrid computing using a neural network with dynamic external memory / DNC](https://www.nature.com/articles/nature20101) | 2016 | 内容寻址、位置关联、空间分配如何一起学？ | 先前核验摘要；待读方法 |
| [Overcoming catastrophic forgetting in neural networks / EWC](https://arxiv.org/abs/1612.00796) | 2016 预印本，2017 PNAS | 如何限制对旧任务重要的参数变化？ | 原始来源、期刊摘要 |
| [Hopfield Networks is All You Need](https://arxiv.org/abs/2008.02217) | 2020 预印本，2021 ICLR | 检索怎样成为能量下降与吸引子收敛？ | 原始摘要、作者介绍 |
| [Learning to (Learn at Test Time): RNNs with Expressive Hidden States / TTT](https://arxiv.org/abs/2407.04620) | 2024 预印本，2025 ICML | 如果 RNN 隐状态就是一个可训练小模型呢？ | 原始摘要、会议、作者代码 |
| [Gated Delta Networks: Improving Mamba2 with Delta Rule](https://arxiv.org/abs/2412.06464) | 2024 预印本，2025 ICLR | 纠错式写入与遗忘门怎样配合？ | 原始摘要、作者代码 |
| [Titans: Learning to Memorize at Test Time](https://arxiv.org/abs/2501.00663) | 2024-12-31 首投，通常归入 2025 | 神经记忆如何结合梯度、动量与遗忘？ | 先前原始摘要、作者介绍 |
| [It's All Connected / MIRAS](https://arxiv.org/abs/2504.13173) | 2025-04-17 | 记忆结构、目标、保留约束、优化器如何统一？ | 原始摘要；待逐式读方法 |
| [Memory-R1](https://arxiv.org/abs/2508.19828) | 2025-08-27；2026-01 有修订 | 如何用回答结果训练记忆操作？ | 原始摘要、作者仓库 |
| [Mem-alpha](https://arxiv.org/abs/2509.25911) | 2025-09-30 | 写入策略如何通过未来问答获得奖励？ | 先前已读方法、训练细节 |
| [Nested Learning / HOPE](https://arxiv.org/abs/2512.24695) | NeurIPS 2025；此 arXiv 版 2025-12-31 | 不同更新频率的优化层级能否形成连续记忆？ | 原始摘要、会议来源 |
| [MemRL](https://arxiv.org/abs/2601.03192) | 2026-01-06；02-12 修订 | 冻结语言模型后，如何从反馈学经验效用？ | 已读检索、更新规则、理论假设 |
| [AtomMem](https://arxiv.org/abs/2601.08323) | 2026-01-13；03-27 修订 | CRUD 操作如何成为可学习的决策空间？ | 原始摘要；待读训练方法 |
| [Learning to Remember / UMA](https://arxiv.org/abs/2602.18493) | 2026-02-13；09-01 修订，页面注明 EMNLP 2026 | 一次构造的记忆，如何服务多个未来问题？ | 摘要、方法概述、评测配置 |
| [Learning What to Remember: Test-Time Training via Context Distillation / TTCD](https://arxiv.org/abs/2608.01672) | 2026-08-03 | 如何用长短上下文差异教快权重记住有用信息？ | 已读方法公式、理论假设、局限 |

这是一张选择性阅读地图，不是截至该日的穷尽综述。摘要核验不等于复现结果。

## 3. 一条可以亲手推导的数学主线

以下首先是教学模型，不是逐字复制任一论文的完整算法。

令键 k、查询 q 属于 R^d，值 v 属于 R^p；记忆矩阵 M 属于 R^(p×d)。读出为：

\[
\hat v=Mq.
\]

### 3.1 追加关联：为什么会互相干扰？

最简单的写入是外积累加：

\[
M_t=\lambda M_{t-1}+\eta v_t k_t^\top.
\]

若 M_0=0，常数遗忘率 lambda 下，查询读出就是：

\[
M_tq=\eta\sum_{i=1}^{t}\lambda^{t-i}v_i(k_i^\top q).
\]

因此“压成一个矩阵”仍然包含按相似度混合历史的效果。不过键相近时，值会串扰；写入数量持续增长，也不会凭空获得无限精确信息容量。这是理解 Fast Weights 和线性关联记忆的起点。

### 3.2 按误差纠正：Delta rule 从哪里来？

对当前键值定义平方误差：

\[
\ell_t(M)=\tfrac12\|Mk_t-v_t\|_2^2.
\]

梯度和一步更新为：

\[
\nabla_M\ell_t=(Mk_t-v_t)k_t^\top,
\qquad
M_t=M_{t-1}+\eta(v_t-M_{t-1}k_t)k_t^\top.
\]

这里写入的是“当前记忆缺少的部分”。若当前映射已经正确，这次更新就为零。它比无限重复追加同一个值更有针对性，但对相近键仍可能产生干扰。

在该简化设置中，对另一个查询 q 的影响为：

\[
\Delta(Mq)=\eta(v_t-M_{t-1}k_t)(k_t^\top q).
\]

这条式子直接说明：一次纠错能否迁移到另一个查询，取决于键和查询的几何关系。Gated DeltaNet 在相关线性状态更新中加入门控；其实际层结构与高效实现需要另读原文，不能把这里的 SGD 演示当完整实现。

### 3.3 从矩阵变成神经网络：TTT 与 Titans

把线性映射 Mk 换成神经网络 f_W(k)：

\[
W_t=W_{t-1}-\eta_t\nabla_W\ell_t(W_{t-1}),
\qquad r_t=f_{W_t}(q_t).
\]

W 是处理当前序列时不断变化的快权重；决定特征与学习机制的慢参数，则在外层训练中学习。理解 TTT 的重点是这两层学习，不是简单地给现有 API 增加一个记忆文件。

Titans 进一步研究梯度信号、动量和遗忘与注意力的组合。它的可更新长期记忆与固定学习的 persistent tokens 是不同组件。

这里最值得质疑的是：重构当前输入的损失，能否教会模型留下未来需要的信息？新奇、预测误差大和长期有用并不是同一个概念。

### 3.4 为“未来有用”寻找训练信号：TTCD

TTCD 让共享模型分别看长窗口和短窗口，在同一位置得到 MLP 输出 y_T、y_S，以停止梯度的长窗口输出监督短窗口：

\[
\ell_t^{CD}=\tfrac12\|y_t^S-\operatorname{stopgrad}(y_t^T)\|_2^2.
\]

其 in-place 实例按 chunk 更新 MLP down-projection 快权重。两种视图均为因果上下文；“长窗口 teacher”不是读取尚未发生的未来。

论文在长上下文任务中报告收益；这不等于已经验证跨月个人工作记忆。它需要模型内部状态和参数访问，还有训练与推理开销。理论迁移论证依赖查询对齐等条件，也不保证任意未来查询都受益。

## 4. 另一条路线：学会管理经验

### Memory-R1、Mem-alpha、AtomMem：学操作策略

可以用一个教学形式理解目标：

\[
\max_\theta\;\mathbb E_{a_t\sim\pi_\theta(\cdot\mid m_t,x_t)}[R_{future}].
\]

动作可以是增删改查。难点是某次写入的收益可能很久后才出现，任务成功也可能来自主模型本身，存在奖励归因问题。

Memory-R1 研究记忆管理与利用；Mem-alpha 重点训练构造策略，并非把检索、回答所有组件一起端到端训练。后者论文实验使用 32 张 H100 训练约 3 天，不能从“记忆模块”这个名字推断训练很轻量。AtomMem 用 SFT 与 RL 学原子操作编排，具体收益范围仍应结合其基准和消融判断。

### MemRL：学经验的效用

它先做语义召回，再结合经验效用选取。实际简化更新形式为：

\[
Q_i\leftarrow Q_i+\alpha(r-Q_i).
\]

这是标量效用的指数移动平均，不是训练语言模型权重。它提出一个很适合讨论的问题：相似的经验是否真的能提高成功率？理论中的平稳奖励等假设，在持续变化的工具环境中需要重新检查。

### UMA：写入如何面对多个未知问题？

UMA 在没有具体未来查询时维护 Memory Bank，之后复用于多个问答；通过同一记忆状态分支出的 QA 轨迹平均奖励监督维护策略。它还保留原始上下文可访问性，因此收益不能简单归因于一个独立、充分的压缩状态。

这种一对多设置比“为眼前这道题做摘要”更贴近持续工作的要求。但是否能迁移到真实编码、任务中断恢复和跨模型调度，仍需单独实验证据。

## 5. 建议阅读次序

1. Fast Weights：先亲手推外积写入和读出，观察串扰。
2. Gated DeltaNet：推平方误差梯度，理解纠错与遗忘。
3. TTT：理解内层快权重和外层训练。
4. Titans：再看非线性记忆、动量及注意力组合。
5. MIRAS：回头统一“结构、目标、保留、更新”四种设计选择。
6. TTCD：研究怎样把“未来有用”变成可计算的监督信号。
7. MemRL 与 UMA 对读：对比经验价值学习与记忆构造策略学习。
8. HOPE：最后看多时间尺度和嵌套优化，避免一开始被宏观概念淹没。

NTM/DNC、Hopfield、EWC 是三条旁支，分别补可微地址、吸引子与容量、参数保护及遗忘。

所需数学可以随阅读补：矩阵外积与范数、链式求导、SGD 与动量、约束优化、概率期望、MDP 与策略梯度；不必先学完所有领域再开始。

## 6. 代码与评测入口

以下代码由作者论文链接或仓库官方声明确认来源；本轮没有安装、运行或复现。

| 入口 | 用途 |
| --- | --- |
| [TTT PyTorch](https://github.com/test-time-training/ttt-lm-pytorch) | 快慢权重、chunk 更新实现 |
| [NVlabs/GatedDeltaNet](https://github.com/NVlabs/GatedDeltaNet) | 门控 delta 更新及高效计算 |
| [TTCD](https://github.com/dangxingyu/ttcd) | 原文链接的上下文蒸馏实现 |
| [MemRL](https://github.com/MemTensor/MemRL) | 两阶段检索和效用更新 |
| [UMA](https://github.com/ictnlp/unified-memory-agent) | 原文链接的记忆构造与训练 |
| [Mem-alpha](https://github.com/wangyu-ustc/Mem-alpha) | 记忆构造策略训练 |
| [Memory-R1](https://github.com/yansikuan/memory-r1) | 记忆管理与利用训练 |
| [LongMemEval](https://arxiv.org/abs/2410.10813) | 长期交互记忆评测起点；2024 预印本 |
| [Memory in the Age of AI Agents](https://arxiv.org/abs/2512.13564) | 分类与更多基准；2025-12 首投，2026-01 修订 |

本轮没有确认 Titans、MIRAS、HOPE 的作者官方实现。网上第三方复现不能默认等价于论文结果。

## 7. 我们自己的工程猜想，尚未得到验证

可以研究一个独立记忆模型：持续吸收工作流，输出主 agent 需要的状态或证据。它是否可行，需要先回答输出接口和训练目标；Titans 的内部隐状态不会自动变成任意 API 模型可理解的上下文。

建议把以下问题留作读论文时的研究笔记，而不是立刻实现的需求：

- 是否需要精确保留路径、标识符和原始证据？连续向量或快权重怎样支持精确回溯？
- 训练信号来自未来预测、未来问答，还是实际工具执行成功？它们优化的能力有何不同？
- 相同实体发生更新时，如何区分纠错、覆盖、历史版本与冲突？
- 固定记忆预算下，如何同时测新知识吸收、旧知识保持和错误传播？
- 更换主模型之后，记忆表示与价值评估是否仍然有效？
- 跨重启保存快权重时，还需保存哪些更新状态？持久化能力与记忆质量要分别验证。

这些问题才是从论文机制走到 smallhead 工程时需要补的证据。当前尚无结论证明某一种记忆形态可以独立满足全部要求。
