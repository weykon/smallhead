# A2A 开发入口与 Lens 实测记录

2026-10-19，真实 MiniMax-M3、本地 HTTP A2A 服务、Node Lens worker。加载顺序为 learning → evidence-check；所有开发操作在临时工作目录进行，没有让被测 Agent 修改 Smallhead 仓库。

## 真实场景

| 场景 | 实际执行 | 结果 |
| --- | --- | --- |
| Rust 所有权笔记 | 21 次模型请求、24 次工具调用，包含搜索、Agent Reach、write/read/edit、Python 检查；Lens 继续 1 次、接受结束 1 次 | 约 135.8 秒完成；文件生成并独立核实存在。超过 HTTP 内联等待后仍通过轮询取得 done |
| Rust 折扣代码修复 | 11 次模型请求、9 次工具调用；先跑失败测试，修改实现，重新运行；Lens 继续 1 次 | 约 36.1 秒完成，原 3 个测试从全部失败变为全部通过。开发 Agent 遇到绝对路径读取错误后改用相对路径继续 |
| 只读继续同一对话 | 新请求没有要求写入/执行权限；最终模型请求不含 write/edit/bash/process_control | 保留上一轮笔记文件名；此轮模型直接回答，没有调用计算工具，不能把它计作工具执行验证 |
| 最后一次读取与讲解 | 新版流解析器，实际 read 代码；3 次模型请求、Lens 继续/结束各 1 次 | 约 7.8 秒完成，最终保留计算逻辑与无答案练习，不再只交付核对报告 |

代码修复完成后，开发者又独立运行 `rustc --test`，确认 3 个原有测试通过。该样例只验证这三个用例，不表示覆盖大整数溢出等所有业务边界。Lens 提示改善了交付形式，但精确字数与事实质量仍取决于模型，不是程序强制保证。

## 本轮修复

- A2A 增加 development 本地能力上限，并按请求 permissions 实际筛选工具；研究/审查子 Monad 仍仅获读取与搜索能力。
- 同一 conversation 顺序执行，取得上一轮完成后的历史；错误原因进入 errors，未被接受的候选和真实工具回执保留。
- 执行与服务管理器在服务器生命周期内共享；健康调度保持运行，退出时取消任务、保存结果并回收受管进程。
- 可选每任务 JSONL 诊断，含模型请求、工具摘要和 Lens 决定；32 MiB 内容上限及省略计数。
- 修正工具参数流：JSON 片段收齐后再解析，删除 provider 重复发送全文的兼容做法；损坏参数不会以空对象进入工具执行。
- Evidence Check 续跑时重述原始任务，保留用户交付目标；修正 Lens 文档中缺失的 serve 子命令。
- DSH 发送工具增加 development 与 conversation_id；queued 在内联等待后获得新的轮询预算，failed 不再被标为成功。

## 自动验证

- core / lens / tui / server：387 passed，2 个原有测试 ignored。
- DSH 发送端：3 passed；覆盖权限参数、对话复用、失败状态和 queued 后轮询，不重发开发请求。
- 新增真实 HTTP 回归覆盖文件读写、命令执行、只读请求过滤、并发对话、收尾失败持久化、常驻服务跨请求存活及退出回收。
- Clippy `-D warnings`、rustdoc `-D warnings`、`git diff --check` 通过。

## 原始证据

临时目录：`/tmp/smallhead-a2a-dev.DN6Fp8`。

- `reply.json`、`followup.json`、`code-fix.json`、`final.json`：真实 HTTP 回复。
- `rust-ownership.md`、`discount.rs`：实际交付文件。
- `sessions/`：包括用户请求、工具回执、候选、Lens 续跑指令和最终答复的快照。
- `diagnostics/task_01a1530a-513f-73e9-8cfd-9639754122ee.jsonl`：笔记开发全过程。
- `diagnostics/task_01a15312-9184-755e-99b5-fd7f03415eb1.jsonl`：代码修复全过程。
- `diagnostics/task_01a15317-5dc1-7141-ab2c-3eea1d78e6e9.jsonl`：最终版本读取与 Lens 续跑。

A2A 与 TUI 目前使用独立服务登记目录；本轮没有把 MCP 或 TUI 交互面板接入 A2A。DSH 修改已做单元验证，正在运行的用户 DSH 实例没有被重启，需要重新加载插件后才使用新发送参数。搜索来源相关性与教学内容的全面评测不在本轮验证范围。

启动与发送示例见 [A2A 协议](a2a-protocol.md)。测试服务器已关闭，临时文件保留供查看。
