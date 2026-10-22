# Laya 动手实验

开发交接与架构结论：[主模型与快速选择模型](../../docs/fast-model-routing.md)。

本地排序模型对照：[Qwen3-Reranker 实验](../qwen-reranker/README.md)。

`selected` 已经是 Laya 内部 argmax 的结果。自定义 query 会移除原案例的预设返回值，以免把原来的 `100*100` 当成新问题的答案；没有 fixture 只表示未配置执行器，不表示没有选中工具。

独立实验，不经过 Cargo、LanceDB 或聊天模型 API。复用 big-head 本机已下载的多语言 Laya 权重及 MLX 环境。

`service.py`、`test_service.py` 和 `requirements.lock` 从相邻 big-head 的 `integrations/laya/` 原样复制，保留 `/health`、`/decide`、`/evaluate`、`/outcome` 协议。新增客户端只调用前两个接口。不修改 big-head 文件。

## 亲手运行

从 small-head 根目录操作。服务已运行时直接进行第二步，不要再启动同端口实例。

```sh
# 终端一：默认复用 ../big-head 的环境及模型，前台服务可用 Ctrl+C 关闭
sh experiments/laya/start.sh

# 终端二：客户端仅需 Python 标准库
python3 experiments/laya/play.py health
python3 experiments/laya/play.py list
python3 experiments/laya/play.py run tool-add
python3 experiments/laya/play.py run memory-project
python3 experiments/laya/play.py run data-live

# 自己换一种说法：不再显示预设正确答案或预设结果
python3 experiments/laya/play.py run tool-add --query '帮我将五乘以六'

# 全部案例 + 候选倒序对照
python3 experiments/laya/play.py bench --reverse
```

输出包括模型选择、预设答案、全部候选概率、HTTP 总耗时、服务推理耗时和模拟回填结果。`fixture result` 是预设数据，不是真实查询或执行；模型选择错时也原样显示，不偷偷用正确标签替换。输入更多参数或复合任务需要真正的业务 Agent，这个实验不模拟它已经实现。

`cases.jsonl` 可直接编辑。`expected` 只用于评估，不发送给模型。三类案例覆盖工具选择、记忆选择和数据源选择，另含复合请求与无合适能力。候选最多七个，服务自动增加 `defer`。原始日志和报告写入 `.smallhead/laya/`。

## 端口与环境

默认 `http://127.0.0.1:3031`，客户端绕过代理并禁止重定向。修改端口：

```sh
sh experiments/laya/start.sh --port 3032
python3 experiments/laya/play.py --url http://127.0.0.1:3032 health
```

也可以设置 `SMALLHEAD_LAYA_URL`。启动脚本可通过 `BIGHEAD_DIR` 或 `LAYA_PYTHON`、`LAYA_MODEL_DIR` 指向其他环境。

如果没有 big-head 环境，在 Apple Silicon 上创建独立环境并安装复制的锁文件：

```sh
uv venv --python 3.12 .smallhead/laya-venv
uv pip install --python .smallhead/laya-venv/bin/python -r experiments/laya/requirements.lock
.smallhead/laya-venv/bin/python experiments/laya/service.py --record-dir .smallhead/laya/service-records
```

最后一个命令首次会下载 pinned checkpoint，已有本地模型请加 `--model-dir`。锁文件来自 big-head 的已验证环境；本轮没有验证全新安装。默认仅支持 MLX，其他硬件需要另行安装 PyTorch 与 laya 后使用 `--backend torch`，本次不声称已验证。

## 本机实测：2026-10-06

模型权重 SHA256：`9d628fd971b700382ac6f65920a86f149777b2e748e0c955fb3b19695aa8f204`。
MLX 0.32.2 / laya-mlx 0.1.0 / GPU FP16，模型上下文上限 1024 tokens。

8 个合成案例各跑正常、倒序两次，共 **10/16 正确，0 传输错误**。HTTP 中位耗时约 **8.97 ms**，首个调用 41.06 ms。每个案例只有一次顺序对照，不是生产准确率，也不是并发或长输入性能测量。

| 案例 | 正常顺序 | 倒序 |
|---|---|---|
| 加法工具 | 错：选乘法 | 错：选乘法 |
| 乘法工具 | 对 | 对 |
| 复合任务应 defer | 错：只选乘法 | 错：只选乘法 |
| 项目记忆匹配 | 对 | 对 |
| 当前偏好 | 错：defer | 错：过期偏好 |
| 实时订单数据源 | 对 | 对 |
| 退货规则资料源 | 对 | 对 |
| 不支持的音乐生成 | 对：defer | 对：defer |

实测原始报告：`baseline-2026-10-06.json`。不要因为输出格式正确、概率高或响应快，就认为语义正确。时间状态和项目权限应尽量先用程序过滤。

## 本地替代方案调研

截至 2026-10-06，尚无本项目实测证据支持“某个替代品全面优于 Laya”。按任务选型更合适：

| 方案 | 适合替代的环节 | 限制 |
|---|---|---|
| [Qwen3-Reranker-0.6B](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B) | 工具描述、记忆、资料的相关性排序 | Apache-2.0，可本地运行；不直接提供完整多类型决策 API，需要适配，拒绝阈值需实测 |
| [MemReranker](https://arxiv.org/abs/2605.06132) | 记忆候选重排，2026 年针对记忆的研究 | 论文结果不等于在本机或中文业务上胜出，权重部署和许可需另核对 |
| [EmbeddingGemma](https://ai.google.dev/gemma/docs/embeddinggemma/model_card) | 先从大量业务／记忆中召回少量候选 | 是 embedding，不是决策模型；需遵循 Gemma 使用许可 |
| 结构化 SQL／规则 | 明确的实体、项目、当前有效版本选择 | 不擅长开放语义，但无需模型推理，是必要基线 |

已补充 Qwen reranker 的本地部署和小样本对照，见本文开头链接；其他替代方案仍是调研，未完成本机对照。复杂规划仍交给业务 Agent。

## 验证

服务测试需要已安装的 MLX 环境；客户端测试可单独用系统 Python。

```sh
python3 -m unittest discover -s experiments/laya -p test_play.py
../big-head/.big-head/laya-venv/bin/python -m unittest discover -s experiments/laya -p 'test_*.py'
```

服务测试会在 loopback 临时端口启动模拟 HTTP server；它们验证协议，不验证模型准确率。准确率来自前面的真实 `bench`。
