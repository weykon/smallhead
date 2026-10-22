# smallhead TUI

助手消息（含流式输出）渲染 Markdown：标题、粗体/斜体、行内代码、列表/任务列表、引用、链接、代码块和表格。窄屏表格改为字段列表，按实际显示行滚动；原始对话和诊断 JSON 保留原文。正文使用终端默认前景色，兼容浅色和深色背景。代码块保留缩进并统一着色，尚无按语言的语法高亮；数学公式保留文本表达。

TUI 默认使用 Monad 业务发现、派发和结果回填。聊天界面与详细事件日志并排显示，F2 切至日志全屏；Ctrl+R 聚焦最近一次完整模型请求。

```sh
cargo run -p smallhead-tui
cargo run -p smallhead-tui -- --prefer-fast=laya
cargo run -p smallhead-tui -- --prefer-fast=qwen
cargo run -p smallhead-tui -- --mock
cargo run -p smallhead-tui -- --resume SESSION_ID
```

真实 MiniMax 模式读取 `MINIMAX_API_KEY`。Laya / Qwen 分别读取 `SMALLHEAD_LAYA_URL` / `SMALLHEAD_QWEN_URL`，默认本机 3031 / 3032。`--mock --prefer-fast=qwen` 使用固定主模型脚本并调用真实本机 Qwen；Mock 不理解用户输入。

输入框 slash 命令：

- `/help`
- `/lens list|load 包目录|reload ID|unload ID`：加载 loop 策略及包内领域工具，下一轮生效；见[包格式、JS API 与示例](../smallhead-lens/README.md)。完整 Learning Agent 领域 worker 位于独立项目，未随本仓库发布。
- 也可直接用自然语言让 agent 通过 `lens` 工具发现、加载、重载或卸载本机包；当前任务的下一次模型调用即取得新工具并继续原任务。发现目录可配置在 `.smallhead/lens-paths.json`，不必每次手动输入 `/lens`。
- `/route large|auto|laya|qwen`：运行中切换后续请求的计算业务偏好，无需重启或设置启动参数
- `/req`：切到最近请求
- `/filter all|request|message|block|tool|monad|route|response|error`
- `/export PATH.jsonl`：新建 JSONL 文件，不覆盖已有文件
- `/cancel`
- `/scrolloff N`：设置日志视窗边距；默认 5，0 表示贴边，选中项仍保持可见
- `/choice-demo`：显示一张本地选择卡片，用于测试键盘高亮选择与点击选择交互
- `/plan`：查看当前任务的完整计划、调整原因及进度
- `/filter plan`：查看计划更新的完整快照
- `/mouse`：查看终端实际传入的鼠标事件数量
- `/list`、`/load SESSION_ID`、`/new`

`/route laya`、`/route qwen` 指定优先后端；`/route large` 沿用主模型提供的表达式。`/route auto` 允许 Agent 在每次 calculation 派发中填写 `model_choice=large_model|laya|qwen`，未填写时沿用主模型。目录的 `supported_fast_backends` 表示业务支持哪些后端，`allowed_choices` 表示当前配置允许哪些选择。服务地址由程序配置，Agent 不填写地址。切换影响下一次聊天请求，不改变已经运行的任务。

可先输入 `/route laya`，再输入「请调用工具计算 7+8」。通过 `/filter route` 查看本轮 `route_configured` 确认配置，再看 `model-route` 的 `backend`、`used`、`fallback` 和 `reason` 确认实际调用与采用情况。要体验逐任务选择，输入 `/route auto`，再请求「重新发现业务目录，选择 Laya 计算 7+8，再沿用主模型派发 1+1」。这是明确指定两种路径的体验案例；不指定后端时 Agent 自行选择，也可以全部沿用主模型。

新增 `fast_decision` 工具可供 Agent 发现并使用 Laya 单选／等级／是非多问题和 Qwen 全排序／前 K 项／筛选／带拒绝单选。它遵守本轮 route 允许的后端，只返回建议；详见 [能力与体验步骤](../../docs/fast-decision-capabilities.md)。

目前仅 calculation 接入业务内的快速方法选择，echo/greeting 仍由本地代码直接执行。主模型负责拆任务、提取表达式；快速模型只选择加减乘除方法，选择与表达式不一致、拒绝或请求失败时复用主模型已提供的表达式，不额外调用大模型。这条链路用于验证职责和路由，不保证整轮更快。

TUI 已接入 `web_search` 和 `agent_reach` 工具。真实模型可按 `web_search.kind=web|wiki|code|news` 搜索外部信息，并在回答里使用结果中的 `provider_used`、`result_count`、`url` 作为依据。Agent-Reach CLI 走 `agent_reach.action=list|search|fetch`，其中 `search` 支持 `search/youtube/bili`，`fetch` 支持 `webpage/rss/youtube/bili`；CLI 未安装或未登录会返回可见错误。用户明确要求安装或配置时，Agent 使用 `bash` 读取安装文档、检查环境、执行安装并验证；普通搜索不会擅自扩展为安装任务。可用示例：「搜索 Rust async runtime，给我 3 条来源摘要」「用 Agent-Reach 搜 YouTube 上 Rust async runtime 的 3 个视频」或「读取这个网页正文：https://example.com」。

复杂任务由 Agent 自行调用 `update_plan` 创建和更新计划，简单问答无需计划。聊天上方显示已完成数量、当前步骤和待办步骤；长计划优先显示当前步骤附近的内容，`/plan` 展示完整内容，可用聊天滚动键浏览。小窗口优先保留聊天和用户选择卡片。步骤状态是 Agent 根据工具结果报告的进度，不代替执行证据；取消、失败会保留未完成步骤并显示中断，正常回复结束也不会自动勾选剩余步骤。每次新请求清除上一轮的进度面板，模型可依据历史重新规划；本版不恢复进度面板，也不是“只规划、不执行”的 Plan Mode。计划快照进入工具返回、诊断日志及正常完成后的会话消息历史。

Agent-Reach 的视频 `fetch` 当前返回 `content_kind=video_metadata`、`transcript_included=false`，仅含标题、简介等元数据，不下载媒体或字幕；不能据此声称已读到完整视频内容。YouTube 等站点仍可能要求登录。可重复验证工具链：`cargo run -p smallhead-tools --example agent_reach_live_smoke -- search`，其他参数包括 `webpage`、`rss`、`youtube`、`bili`、`youtube-search`、`bili-search`；RSS 可再传一个 feed URL。

键位：F1 / Ctrl+F1 聊天、F2 日志、Ctrl+P / F3 进程（↑↓ 选择，Esc 返回）、Tab 补全命令或切换左右焦点、↑↓ 滚动聊天或选日志、PgUp/PgDn 滚动详情、Home/End 浏览最早事件或恢复跟随、Ctrl+F 切过滤、Ctrl+R 最近请求、Ctrl+X 取消、Ctrl+C 退出。选择卡片出现时，↑↓ 高亮选项，Enter 确认。

鼠标：点击日志选中并重置详情滚动位置；滚轮按指针所在区域滚动聊天、移动日志选择或滚动日志详情。顶部提供明确的 `[聊天]`、`[日志]`、`[发送]`、`[取消]` 按钮；输入框点击只聚焦，避免误提交草稿，状态栏仍可点击取消。跟随模式下，日志足够多时，默认在最新项下方持续留出 5 行空白，不会每五条事件循环跳回底部；小窗口自动缩小边距。

用户选择：Agent 可调用 `ask_user_choice` 发出结构化选择请求，TUI 在聊天页底部显示卡片按钮；用户用 ↑↓ 高亮选项并按 Enter 确认，也可以点击选项。选择结果以 `selected_option_id` 和结构化 `intent` 回到工具调用。`/choice-demo` 可在不调用模型的情况下测试同一套 UI。

iTerm2 若点击或滚轮没有反应，先操作鼠标再输入 `/mouse`：计数为 0 表示程序没有收到鼠标事件。到 Settings → Profiles → Terminal 检查 Enable mouse reporting、Report mouse clicks & drags 和 Report mouse wheel events。按住 Option 会临时禁用鼠标报告，让终端进行文本选择。参见 [iTerm2 官方设置说明](https://iterm2.com/documentation-preferences-profiles-terminal.html)。程序已在启动时开启鼠标捕获，不会修改 iTerm2 配置。

大终端为左聊天、右日志；窄终端优先显示聊天，F2 打开完整日志页。日志记录任务 ID、请求消息、工具定义、模型返回内容块、工具参数和真实结果、Monad handoff/result、快速路由原始响应。HTTP 请求体可查看；鉴权头和 API key 不记录。日志保存在内存，最多保留 3000 条，超出最旧事件会淘汰。`/export` 只导出当前仍保留的记录；这些内容可能包含用户对话或业务数据，请选择合适的保存位置。

整轮默认 120 秒超时。Ctrl+X 或 Ctrl+C 会停止等待当前请求；已经执行的外部副作用不会自动回滚。本轮被取消或超时后，不把半截 assistant/tool 调用写入对话上下文；诊断日志仍保留。每轮新建主任务 ID；恢复会话只恢复模型消息历史，不恢复旧任务执行状态。

源码划分：`main.rs` 只处理启动与终端生命周期；`app.rs` 管输入、会话和界面状态；`runtime.rs` 创建 MainMonad 后台任务；`render.rs` 只负责绘制；`logs.rs` 负责事件模型、筛选和导出。core 的 `diagnostics.rs` 用 task-scoped channel 传递结构化事件，不会把诊断文本打印到 TUI 屏幕外。
