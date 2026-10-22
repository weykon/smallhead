# Qwen3-Reranker-0.6B 本地部署

开发交接与架构结论：[主模型与快速选择模型](../../docs/fast-model-routing.md)。

已安装 Ollama 社区量化模型，但实际验证发现 Ollama 路径不能可靠返回每个输入的完整 yes/no 概率。因此默认使用官方原始权重的本地评分服务 `http://127.0.0.1:3032`。不需要 API key，不影响 Laya 的 3031 端口。

## 立即体验

从 small-head 根目录执行。服务已启动时不需要重复启动。

```sh
# 服务重启时使用；复用 big-head 的 torch/transformers 环境
sh experiments/qwen-reranker/start.sh

# 另一个终端：比较工具相关性
python3 experiments/qwen-reranker/rerank.py \
  --instruction 'Select the tool that fulfills the query.' \
  '帮我将五乘以六' \
  'multiply: Multiply two numbers 乘法' \
  'add: Add two numbers 加法' \
  'read_order: 查询订单'

# 记忆／资料同样可以排序
python3 experiments/qwen-reranker/rerank.py \
  'small-head 使用什么语言？' \
  'small-head 使用 Rust。' \
  '另一个 web-shop 项目使用 TypeScript。'

python3 experiments/qwen-reranker/bench.py
```

第一次示例实测 multiply=0.42836、add=0.03406、read_order=0.000247，首次三候选服务总耗时约 1614ms，包含首次推理预热。这不是稳定延迟 benchmark。`baseline-2026-10-06.json` 保存与 Laya 同案例的诊断结果，不能将不同协议的小样本直接推广成优劣结论。

分数来自官方提示词下 `softmax([logit(no), logit(yes)])[yes]`。每个候选独立评分，分数不要求加起来等于 1，不是业务正确率。取 max 可以得到排序第一项，但不证明它足够相关，也不完成参数抽取或工具执行。

服务使用 PyTorch 2.8.0、Transformers 4.57.6、Apple MPS、FP32，逐候选计算，避免首次实现引入批处理内存问题。输入超过 8192 tokens 明确拒绝，不静默裁剪。最多 32 候选，服务忙时返回 503。

官方权重已下载到 `.smallhead/qwen-reranker/model`。启动脚本支持 `QWEN_PYTHON` 环境变量，也可传 `--model-dir` 和 `--port`。依赖来自现有环境，本轮未改动其包版本。

## Ollama 兼容性记录

本机 Ollama 0.24.0；下载并验证了两个社区模型（不是 Qwen 官方 Ollama 发布）：

- `dengcao/Qwen3-Reranker-0.6B:Q8_0`（639MB）：实测出现均匀首 token 概率，未通过评分检查。
- `ExpedientFalcon/qwen3-reranker:0.6b-q8_0`（639MB）：部分输入正常，但 top-20 有时没有小写 yes 或 no，不能恢复标准分数。

可选诊断命令：

```sh
python3 experiments/qwen-reranker/rerank.py --backend ollama \
  'What is the capital of China?' 'The capital of China is Beijing.'
```

客户端使用 raw prompt、一个输出 token 和 logprobs，不把自由生成的数字当作分数，也不把缺失的 token 设成零。缺失时会报错，请使用默认 native 服务。没有用大小写混合概率或伪造分数掩盖转换／协议差异。

## 本地 HTTP API

```sh
curl http://127.0.0.1:3032/health
curl http://127.0.0.1:3032/rerank \
  -H 'Content-Type: application/json' \
  -d '{"query":"订单是否发货","documents":["实时订单数据库","历史聊天记忆"]}'
```

只返回排序及原始候选索引，不执行任何工具。

## 测试与依据

```sh
python3 -m unittest discover -s experiments/qwen-reranker -p 'test_*.py'
```

- [Qwen 官方模型卡和评分代码](https://huggingface.co/Qwen/Qwen3-Reranker-0.6B)
- [Ollama generate / raw / logprobs](https://docs.ollama.com/api/generate)
- [社区转换一](https://ollama.com/dengcao/Qwen3-Reranker-0.6B:Q8_0)
- [社区转换二](https://ollama.com/ExpedientFalcon/qwen3-reranker)
