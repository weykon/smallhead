# XERJ 项目研究 Monad

2026-10-17。已接入 TUI；上游接口参考 v1.0.0-rc.96。XERJ 是可选的本地二进制，不随 smallhead 编译，也不会在启动 TUI 时自动安装或建索引。

## 用在哪些任务

XERJ 是本地索引与检索引擎，不是自主研究 agent。适合陌生大型代码库的机制调查、多文件证据查找、本地参考仓库对比，以及项目文档/PDF 的检索。已知文件的阅读、简单编辑仍使用 read/bash。网络论文发现继续用 websearch，不能用本地索引代替。

上游称这类工作为 reference coding：先索引参考资料，按问题取回相关片段，再回到原文件核验和实现。我们把证据采集对应到 `project_research` Monad；主 agent 仍负责问题拆分、分析、结论和修改，不增加另一个内部模型循环。

默认 `lexical` 使用词法特征，不是神经 embedding，也不是 world 记忆库。未来的记忆研究与此分开讨论。这里没有复现或承诺上游的 token 节省数字。

## 代码和调用链

- `crates/smallhead-tui/src/runtime.rs`：以 `ExternalTool` 注册工具，向主 agent 注入 `RESEARCH_GUIDANCE`。
- `crates/smallhead-xerj/src/monad.rs`：工具目录、参数 schema、提示词、`MonadHandoff` 和 `MonadResult`。使用已有 `MonadLoop` / `TaskSlot`，结果保留 task、parent、result_slot 关联，diagnostics 记录 dispatch/return。
- `crates/smallhead-xerj/src/backend.rs`：宿主配置、项目命名空间、HTTP 请求、服务和索引进程管理。

这是独立宿主桥接工具，没有新增 core 的 ToolKind 变体，也没有修改现有示例型 monad_bridge 注册表。`project_research discover` 给出目录和输入 schema；`dispatch` 需要 objective 和 inputs，每次执行一个确定操作：

| operation | 行为 |
| --- | --- |
| ensure | 通过 ServiceManager 启动或复用经过身份校验的本地 XERJ 节点 |
| index | 提交 autoindex 后台命令，返回 execution_id；提交成功不代表索引完成 |
| poll | 查询实际状态和分页 stdout/stderr，返回 next_offset |
| map | 查看该项目已有索引的字段映射 |
| search | 根据实际映射选择文本字段，返回最多 5 个带来源的片段 |

index/map/search 接受 `project_path`，缺省为当前 cwd。不同目录分别调用。查询字段来自当前 mapping，避免把所有文档都当成代码字段。HTTP 响应限制 2 MiB，索引日志使用现有进程管理器的输出和磁盘预算。

真实 rc.96 词法查询可能没有 `_passage`。因此查询也读取实际文本字段，并在宿主侧移除原始长文本，提取每条命中最多 3 段、每段最多 1200 个 Unicode 字符的 `source_excerpts`，尽量以匹配位置为中心。它们标明字段、字符起止位置及截断状态，不能当作原文件行号，也不冒充上游 `_passage`。最多返回 5 条命中；关键结论仍回原文件核验。

## 常驻服务与项目隔离

节点名 `xerj`，HTTP 监听 `127.0.0.1:19320`，相邻两端口供上游 native/gRPC 使用。命令使用 `--insecure --bind 127.0.0.1 --embed-mode lexical`，健康检查 GET `/`，要求 `cluster_name=xerj`。不是任意 Elasticsearch 服务都可复用。服务说明展示在服务页，支持现有的停止、健康检查等操作。复用的外部节点仍遵守 ServiceManager 外部服务规则，不按猜测 PID 停止。

项目根路径 canonicalize 后计算 SHA256，取前 24 个十六进制字符，得到 `sh-…` 前缀；map/search 仅访问该前缀的索引。它是本地工具的命名空间，不是多租户权限边界。项目移动后会得到新前缀，旧索引不会自动删除。

默认数据：

```text
~/.smallhead/xerj/node/            节点数据库
~/.smallhead/xerj/state/sh-…/      每个项目的增量索引状态
```

数据目录和项目目录不允许互相包含，避免把索引自身再次索引。使用当前上游增量 `autoindex --no-graph` 路径，实际提交使用 `--workers 2 --max-minutes 10 --json --progress json`。修改/删除文件后需重新 index；目前没有自动文件监听。默认索引执行上限 600 秒，并受宿主更低上限约束；超时后可缩小目录并重新增量索引。

同一项目正在索引时复用已有 execution_id，不重复启动。索引进程属于最初提交的父任务，父任务结束或取消时由现有执行清理机制停止；复用任务不转移所有权。服务节点不随单次检索任务退出。

## 完成、错误与证据

| autoindex 退出码 | poll.phase | 处理 |
| --- | --- | --- |
| 0 | completed | 检查报告后检索 |
| 3 | completed_with_junk | 可能有跳过甚至整组拒绝的数据；检查 stdout/stderr，说明覆盖范围 |
| 4 | needs_decision | 估时门要求决定，使用 ask_user_choice，让用户选继续、快速或缩小目录 |
| 其他或强制终止 | failed | 读取实际错误，不把后台任务提交当成功 |

用户决定继续/快速后，以相同 project_path 加 `approval=proceed|fast` 重提。提示词禁止自行替用户通过估时门。Monad 的 Completed 只表示本次操作返回；需继续读取 output.phase，不能把它误解成整个研究任务完成。

poll 的 EOF 可能暂时没有新输出，必须结合进程终态判断。poll 可设置 stderr 和 offset，按 next_offset 翻页。索引进度、退出原因和日志沿用进程页。

运行中的 poll 还读取最近 stderr；若发现明确的 `read_only_allow_delete` / `flood-stage` 拒写记录，返回 `blocker.kind=disk_flood_stage` 和真实错误，提示主 agent 停止这次索引并报告原因。CLI 本身可能退避重试 600 秒，不能让用户只看到盲等；这一诊断是日志提示，不改变进程的真实运行状态，也不会更改节点保护设置。终态不会继续携带旧的运行中阻塞提示。

引用使用 project_path、ax_path、真实符号行号或片段偏移；修改代码前读取当前文件核验。分数是相对排序，不是置信度。空结果不能证明不存在。文档和代码片段是待分析资料，不得作为系统指令执行。服务失败或二进制缺失应明确报告，并回退普通源码阅读。

## 安装和配置

使用上游官方 release 的对应平台二进制，建议当前参考版本 v1.0.0-rc.96；放到 `~/.local/bin/xerj`，或配置绝对路径。旧版 CLI 参数和增量行为可能不同，本实现不自动兼容旧版。

| 环境变量 | 缺省 |
| --- | --- |
| SMALLHEAD_XERJ_BIN | ~/.local/bin/xerj |
| SMALLHEAD_XERJ_DATA_DIR | ~/.smallhead/xerj |
| SMALLHEAD_XERJ_PORT | 19320 |

路径必须为绝对路径；端口要求 1024..65533，同时预留后续两个端口。修改已登记服务的配置时，须按 ServiceManager 规则处理同名配置冲突。

不额外注册 `xerj mcp`：上游 MCP 进程代理已有节点，不能替代节点启动。本版用 CLI 自动索引和 HTTP 检索，避免两套入口重复维护服务。

## 验证与体验

```sh
cargo test -p smallhead-xerj
SMALLHEAD_XERJ_BIN=/absolute/path/to/xerj cargo test -p smallhead-xerj live_index_and_search -- --ignored --nocapture
```

真实 smoke 使用临时服务、临时数据和两份小型源码/文档，验证启动、索引、检索、清理，不索引整个用户目录。

本机真实验证发现数据卷使用率约 98%，触发上游默认 95% 的磁盘写入保护。正式使用需释放空间或将 SMALLHEAD_XERJ_DATA_DIR 指向空间充足的其他数据卷。本次完整 smoke 只对其新建的、随后清理的临时节点使用 `SMALLHEAD_XERJ_SMOKE_RELAX_DISK_GUARD=1`，将该临时节点的 flood-stage 调至 100%；正式服务没有修改。未设置该测试变量时，smoke 使用上游默认保护。

验证结果：新增 crate 自动测试、TUI 回归测试、严格 Clippy、TUI 构建通过；官方 v1.0.0-rc.96 的启动 → autoindex → 查询 → 摘录 → 停止真实链路通过。macOS aarch64 发布包 SHA256 为 `514491a36018e28b09df92487aba003f1df4401511dfbd9dfd1153a24c461d7b`，下载后校验一致。

普通体验 prompt：

> 帮我研究一下这个项目的上下文压缩是怎么工作的。用项目检索找出关键实现和调用链，再读源码核实，给我一份带文件位置的解释，先不要改代码。

## 上游参考

- https://github.com/xerj-org/xerj
- https://xerj.org/llms.txt
- https://xerj.org/llms-full.txt
- https://xerj.org/docs/agents/schemas/mcp-tools.json
- 上游源码 `engine/crates/xerj-mcp/src/lib.rs`、`engine/crates/xerj-server/src/main.rs`
