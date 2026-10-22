# smallhead Roadmap — v0.1 → v1.0

> 详细任务拆解。每个 v 版本都有 **GIVEN/WHEN/THEN** 验收标准。
> 任务粒度：每条 1 个具体可执行 task，可由单个工程师 0.5–2 天完成。

---

## v0.1 — MVP（6 周）

**目标**：跑通最小 agent loop —— Anthropic provider + 5 个内置工具 + JSONL 会话 + 单次 `run` 命令。

### Tasks

1. **Crate workspace 初始化** — 写 9 个空 `Cargo.toml`（`crates/smallhead-{core,tools,permission,session,ecs,skill,cli,server,web}/Cargo.toml`），每个含 `[package] name edition = "2021" version = "0.1.0"` + workspace 依赖。
2. **`smallhead-core` 包结构** — `src/lib.rs` 暴露 `pub mod message; pub mod event; pub mod tool; pub mod provider; pub mod agent_loop;`（先 `mod` 占位即可）。
3. **`AgentMessage` enum** — 按 ARCHITECTURE §3.1 实现 9 个变体（`User`/`Assistant`/`ToolResult`/`Notification`/`Compaction`/`BranchMarker`/`Steering`/`FollowUp`/...） + `#[serde(tag = "role", rename_all = "snake_case")]`。
4. **`ContentBlock` enum** — 5 个变体（`Text`/`Image`/`Thinking`/`ToolUse`/`ToolResult`） + `#[serde(tag = "type", rename_all = "snake_case")]`。
5. **`ToLlmMessage` trait** — `fn to_llm(&self) -> Option<LlmMessage>`，对每个 variant 实现（`Notification`/`Steering`/`FollowUp`/`Compaction`/`BranchMarker` 返回 `None`）。
6. **`Provider` trait** — ARCHITECTURE §3.2：`id()`/`model()`/`context_window()`/`stream()`/`cache_key()` + `ProviderRequest`/`ProviderEvent` 类型。
7. **`LlmMessage` 类型** — 简化版的 wire 消息（`{role: "user"|"assistant", content: ...}`），用作 LLM 边界。
8. **`ToolKind` enum + dispatch** — ARCHITECTURE §3.3：5 个 builtin + `Dynamic(Box<dyn DynamicTool>)` + `Skill` placeholder。
9. **`ToolSpec` + JSON Schema** — `name/label/description/input_schema: serde_json::Value/output_schema`；`ToolKind::spec()` 返回 `ToolSpec`。
10. **`ToolContext` 类型** — `pub struct ToolContext { pub session_id: String, pub cancel: CancellationToken, pub world: Arc<Mutex<World>>, pub permission: Arc<Ruleset> }`。
11. **`smallhead-tools` 包结构** — `src/{bash,read,edit,write,webfetch}.rs` + `mod.rs` dispatcher。
12. **`BashTool::execute`** — `tokio::process::Command` 调用，stdout/stderr 合并，`max_duration = 30s`，`max_output_bytes = 1MB`，超限截断。
13. **`ReadTool::execute`** — 接受 `path` + `offset?` + `limit?`，读文件返回 `Vec<ContentBlock::Text>`。
14. **`EditTool::execute`** — `old_text` + `new_text` 精确字符串替换；找不到 → 报错。
15. **`WriteTool::execute`** — 接受 `path` + `content`，创建/覆盖文件。
16. **`WebFetchTool::execute`** — `reqwest::get(url)`，`max_response_bytes = 5MB`，返回 text，`max_duration = 15s`。
17. **`smallhead-permission` 包结构** — `src/{rule,ruleset,pattern,doom_loop,decision}.rs`。
18. **`Permission` enum** — 7 个变体（`Bash`/`Read`/`Write`/`Edit`/`Webfetch`/`EcsRead`/`EcsMutate`/`Custom(String)`）。
19. **`Pattern` enum** — 4 个变体（`Exact`/`Prefix`/`Glob`/`CommandShape`）+ glob crate 集成。
20. **`Rule` + `Ruleset`** — `Vec<Rule>`，`evaluate(perm, input, ctx) -> Decision` last-match-wins。
21. **`Decision` + `Action`** — `enum Action { Allow, Ask, Deny }` + `Decision { action, reason }`。
22. **`DoomLoopDetector`** — ARCHITECTURE §6.3：`record(name, args)` + `triggered() -> bool`，阈值默认 3，hash 用 `sha2` 截前 8 字节。
23. **`smallhead-session` 包结构** — `src/{entry,store,sqlite_index,compactor}.rs`。
24. **`Entry` + `EntryKind`** — ARCHITECTURE §7.2：11 个变体（`Message`/`Compaction`/`BranchMarker`/`ProviderTurn`/`ToolCall`/`ToolResult`/`Checkpoint`/`WorldMutation`/`DoomLoopTriggered`/`PermissionDecision`/`Error`）。
25. **`SessionStore::append`** — 打开 `entries.jsonl` 追加一行，`fsync` 每 5 条或 1s。
26. **`SessionStore::read_all`** — 从 JSONL 读取所有 entries，反序列化为 `Vec<Entry>`。
27. **SQLite index schema** — ARCHITECTURE §7.3：`entries` 表 + 3 个索引 + FTS5 虚表。
28. **`SessionStore::rebuild_index`** — 启动时如果 SQLite 缺，从 JSONL 全量重建。
29. **`SessionStore` 文件路径** — `~/.local/share/smallhead/sessions/{cwd-hash}/{session-id}/entries.jsonl`。
30. **`uuid v7` 生成** — `Entry::id` 用 `Uuid::now_v7().to_string()`。
31. **`smallhead-cli` 包结构** — `src/{main,run,tui,commands}.rs`，`bin = "smallhead"`。
32. **`smallhead run` 子命令** — 接受 `prompt` 参数（位置），`--model`、`--provider`、`--session` flag。
33. **`smallhead resume` 子命令** — 接受 `session-id` 参数，从磁盘加载 JSONL 续跑。
34. **`smallhead sessions` 子命令** — 列出 `~/.local/share/smallhead/sessions/` 下所有 session（按 mtime 倒序）。
35. **`smallhead version` 子命令** — 打印 `CARGO_PKG_VERSION` + git SHA。
36. **`AgentLoop::run_one_turn`** — ARCHITECTURE §5.1：内层 while，处理 tool calls + steering。
37. **`AgentLoop::run` (outer)** — 持续处理 follow-up 队列直到空。
38. **`transform_context`** — 把 `Vec<AgentMessage>` → `Vec<AgentMessage>`（修剪 + 注入；目前是 identity placeholder）。
39. **`collect_assistant`** — 把 `ProviderEvent` 流累积成 `Assistant` message。
40. **`execute_tool` 分发** — 根据 `ToolKind::spec().name` 路由到对应 builtin 执行器。
41. **`commit_message`** — 调 `SessionStore::append(EntryKind::Message(msg))`。
42. **steering_tx/follow_up_tx channel** — `AgentLoop` 持有 `mpsc` sender，外部 `steer()`/`follow_up()` 投递。
43. **Anthropic provider 实现** — ARCHITECTURE §9.1：`AnthropicProvider` + SSE 解析 → `ProviderEvent`。
44. **SSE 事件解析** — 用 `reqwest::Response::bytes_stream` + 手动 SSE 解析（不引第三方 SSE crate）。
45. **ProviderRequest → Anthropic JSON** — `to_anthropic_tool` + `to_anthropic_msg` 转换函数。
46. **`AnthropicProvider::cache_key`** — sha256 over serialized `ProviderRequest.messages`。
47. **`AnthropicProvider::stream` cancel** — 监听 `CancellationToken`，中途 abort `reqwest::Request`。
48. **Mock provider** — `MockProvider` 接受 `Vec<ProviderEvent>` 回放（ARCHITECTURE §13.4）。
49. **`smallhead.toml` 配置加载** — `directories::UserDirs` + `serde_yaml` 或 `toml` crate（用 `toml`）。
50. **`[provider]` 配置节** — `primary` + `api_key_env` + `model` + `base_url`。
51. **`[permission]` 默认 ruleset** — `Bash *=allow` / `Bash rm *=ask` / `Bash sudo *=deny` / `Read .env=deny`。
52. **CLI entry: clap derive** — `#[derive(Parser)] enum Cmd { Run(RunArgs), Resume(ResumeArgs), Sessions, Version, ... }`。
53. **CLI 错误处理** — `anyhow::Result` + `colored`/pretty error 输出。
54. **单元测试: `AgentMessage::to_llm` 过滤** — `Notification` 返回 `None`，`User` 返回 `Some`。
55. **单元测试: `Ruleset::evaluate` last-match-wins** — 3 条规则验证后定义的覆盖前定义的。
56. **单元测试: `DoomLoopDetector::triggered`** — 连续 3 次相同 → true；不同 → false。
57. **单元测试: `SessionStore::append` + `read_all` 往返** — 100 entries round-trip 一致。
58. **集成测试: `MockProvider` 单 turn** — 用 Mock 流构造一个简单 user→assistant→tool→tool_result→assistant 对话，跑 `AgentLoop::run_one_turn`，验证 entries.jsonl 内容。
59. **集成测试: `BashTool` happy path** — `echo hello` → 工具输出含 "hello"。
60. **集成测试: `EditTool` 字符串替换** — 临时文件 + old_text/new_text 替换成功。
61. **`agent_loop` step() 纯函数** — ARCHITECTURE §5.2：`step(state, event) -> (LoopState, Vec<Action>)` 无 I/O。
62. **测试: `step()` pure pipeline** — 不 mock 文件系统，仅 state 转换。
63. **CI: cargo fmt + clippy + test** — `.github/workflows/ci.yml`（或本地 `justfile`）跑 `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`。
64. **`README.md` quick-start** — `cargo run -- run "..."` 三行示例。

### Acceptance

**GIVEN** 我已经 build 完 workspace 并设置了 `ANTHROPIC_API_KEY`，
**WHEN** 我执行 `cargo run -- run "create a hello world in /tmp/hw.rs"`，
**THEN**
- `AgentLoop` 调用 Anthropic provider（claude-sonnet-4-6 或最新），发一条 user message，
- LLM 返回 `Bash` tool call，`BashTool` 执行 `cat > /tmp/hw.rs <<EOF ... EOF`，
- LLM 返回最终 assistant 文本（无 tool_calls），loop 退出，
- `/tmp/hw.rs` 文件存在且内容正确，
- `~/.local/share/smallhead/sessions/<hash>/<sid>/entries.jsonl` 包含 ≥ 5 个 entry（user / assistant / tool_call / tool_result / final assistant），
- SQLite 索引包含等量 entries，
- `cargo test --workspace` 100% pass。

---

## v0.2 — ECS 集成 + TUI + Compaction（4 周）

**目标**：引入 ECS kernel（11 world ops）+ ratatui TUI + pi 5 步 compaction。

### Tasks

65. **`smallhead-ecs` 包结构** — `src/{world_op,checkpoint,vendor}.rs`。
66. **vendored bevy_ecs** — `vendor/bevy_ecs-0.16/` 子模块（path 依赖）。
67. **`WorldOpKind` enum** — 11 个变体（`QueryEntities`/`GetComponent`/`InsertComponent`/`RemoveComponent`/`UpdateComponent`/`SpawnEntity`/`DespawnEntity`/`AddRelation`/`RemoveRelation`/`DefineComponent`/`RunSystem`）。
68. **`WorldOpTool` struct** — `kind + world: Arc<Mutex<World>> + permission: Permission`。
69. **`WorldOpTool::spec()`** — 每个 kind 输出对应 JSON Schema（`input_schema` 用静态 schema 字符串）。
70. **`WorldOpTool::execute`** — match on kind，调对应 ECS 方法。
71. **`QueryEntities` 实现** — 接 `query: String`（伪组件表达式），返回 JSON 数组。
72. **`GetComponent`/`InsertComponent`/`RemoveComponent`/`UpdateComponent`** — 对单 entity 的 component 操作。
73. **`SpawnEntity`/`DespawnEntity`** — 创建/销毁 entity，绑初始 components。
74. **`AddRelation`/`RemoveRelation`** — graph edge 操作（用 bevy_ecs 的 relationship 系统）。
75. **`DefineComponent`/`RunSystem`** — schema + system 注册（最小可用即可）。
76. **`Permission::EcsRead`/`EcsMutate`** — 加到 `Permission` enum。
77. **默认 ruleset 增加 ECS 条目** — `EcsRead *=allow`、`EcsMutate *=ask`。
78. **`WorldCheckpoint` struct** — ARCHITECTURE §4.3：`world_id/version/world: serde_json::Value/action_log_digest/timestamp`。
79. **`checkpoint_every_n_actions`** — 每 100 次 mutation 触发 snapshot。
80. **`AnchorRef` 类型** — `struct AnchorRef { entry_id: String, kind: String, quote: String }`。
81. **`Compactor` struct** — ARCHITECTURE §7.4：`context_window/reserve_tokens/summary_model`。
82. **`Compactor::find_cut_point`** — 从尾部向前算 token，找到 `total - (window - reserve)` 位置。
83. **`Compactor::extract_anchors`** — 扫描 entries，提取所有 `ToolCall`/`ToolResult`/`WorldMutation` 的 entry id（保留被引用的）。
84. **`Compactor::summarize`** — 调用 haiku provider，返回 `summary: String + anchor_refs: Vec<AnchorRef>`。
85. **`Compactor::compact`** — 编排上述 4 步 + append `EntryKind::Compaction`。
86. **token 估算** — 简单字符数 / 4 近似（避免引 `tiktoken-rs`）。
87. **`Compactor::restore_for_prompt`** — 从 `first_kept_entry_id` 起 + summary block 重建 context。
88. **自动 compact 触发** — `transform_context` 前检查 token 估算，超阈值调 `Compactor::compact`。
89. **ratatui TUI 入口** — `smallhead tui` 子命令（或无 subcommand 默认进入）。
90. **TUI 双栏 layout** — 左对话流（可折叠）+ 右侧 token budget + world state panel。
91. **Token budget 可视化** — 右侧 panel 显示 `used/window/cache epoch`。
92. **World state 可视化** — 右侧 panel 显示 `entities/components/systems` 计数。
93. **TUI 消息折叠** — Tab 键折叠/展开 tool 输出。
94. **TUI 输入框** — 底部 prompt 输入区。
95. **Keybindings** — ARCHITECTURE §10.3：Enter=steer、Alt+Enter=follow-up、Ctrl+T=tree、Ctrl+P=provider、Ctrl+K=compact、Ctrl+C=interrupt。
96. **steer / follow_up UI hook** — 输入框 Enter/Alt+Enter 分别 send `steering_tx`/`follow_up_tx`。
97. **tui 渲染 60fps** — 用 `crossterm::event::poll(16ms)` + `ratatui::Terminal::draw`。
98. **session 树视图 (CLI)** — `smallhead tree <session-id>` 输出文本树（`└─`/`├─` ASCII）。
99. **session 树视图 (TUI Ctrl+T)** — 弹 panel 显示当前 session 的 entries 树状图。
100. **测试: `WorldOpTool::execute` 11 类** — 每类至少 1 个单元测试。
101. **测试: `Compactor::compact` round-trip** — 100 entries → compact → restore_for_prompt → 验证 anchor_refs 覆盖所有引用 entry。
102. **测试: `Compactor` 不丢原话** — compact 后原始 entries 仍可在 JSONL 中找到。
103. **测试: TUI 渲染（无 I/O 验证）** — 把 `LoopState` 喂入 render 函数，验证 buffer 包含期望 widget。
104. **集成测试: end-to-end with ECS** — Mock provider 给出 `world_op: query_entities` → tool 执行 → 返回结果 → LLM 收到 tool_result。

### Acceptance

**GIVEN** v0.1 已完成且我有 v0.2 build，
**WHEN** 我在 ratatui TUI 里输入 "add a User entity with name='alice' to world"，
**THEN**
- LLM 决定用 `WorldOpKind::SpawnEntity` tool，
- `EcsMutate` permission 评估 → Ask（用户按 y 确认），
- ECS World 增加一个 entity，components 包含 `name="alice"`，
- 右侧 World state panel 实时更新（entities count +1），
- 右侧 Token budget panel 显示当前 token 估算，
- 当对话达到 80% of context_window → 自动触发 compaction，anchor_refs 写入 entries.jsonl，
- `smallhead tree <session-id>` 输出树状图，compaction entry 显示为分支节点。

---

## v0.3 — 多 provider + Skills + DoomLoop 集成（4 周）

**目标**：补齐 OpenAI/Ollama/Google provider + Skill loader + 对抗测试。

### Tasks

105. **`smallhead-provider-anthropic` 拆 crate?** — 暂留 core 内（v0.3 暂不拆）。
106. **`OpenAiProvider` 实现** — `Provider` trait + OpenAI Chat Completions API 适配。
107. **`OpenAiProvider` tool calling** — `tools: [{type: "function", function: ...}]` 映射。
108. **`OpenAiProvider` 流式** — SSE 解析 OpenAI `data: {...}` 格式。
109. **`OllamaProvider` 实现** — `/api/chat` endpoint + 流式 NDJSON。
110. **`OllamaProvider` 本地 base_url** — `http://localhost:11434` 默认。
111. **`GoogleProvider` 实现（Gemini API）** — `/v1beta/models/{model}:streamGenerateContent`。
112. **`Provider` 注册表** — `ProviderRegistry { primary: Box<dyn Provider>, fallbacks: Vec<Box<dyn Provider>>, haiku: Box<dyn Provider> }`。
113. **`smallhead.toml` 多 provider** — `[provider.openai]` / `[provider.ollama]` / `[provider.google]` 配置节。
114. **`Ctrl+P` 切 provider** — TUI 按键循环切换 primary provider。
115. **`smallhead-skill` 包结构** — `src/{loader,frontmatter,registry}.rs`。
116. **agentskills.io frontmatter 解析** — `serde_yaml` 解析 SKILL.md 顶部 YAML。
117. **Skill 目录扫描** — `~/.smallhead/skills/` + `./.smallhead/skills/`（项目级覆盖）。
118. **`Skill` registry** — `HashMap<String, Skill>` 按 name 索引。
119. **`load_skill` tool** — 接受 `name: String`，从 registry 取 SKILL.md 内容，按需注入 context。
120. **`SkillTool` 在 `ToolKind` 注册** — 出现在 LLM tool list。
121. **Skill description 作为路由信号** — LLM 看到所有 skill 的 `name + description`，按需调 `load_skill`。
122. **Skill 缓存** — `SkillRegistry` 用 `Arc<RwLock<...>>`，扫描结果内存缓存。
123. **Skill `description` 验证** — 启动时 warn 模糊或过短的 description（参考 pi 实践）。
124. **`smallhead skills` 子命令** — 列出所有已加载 skill + 路径 + description 摘要。
125. **doom_loop 触发集成测试** — 构造 MockProvider 连续给同一 tool call 3 次，验证 N+1 次时被 ask。
126. **doom_loop 阈值可配** — `[permission] ask_threshold = N` 配置。
127. **doom_loop 用户可覆盖** — TUI 弹窗 "doom_loop detected, continue?" y/n。
128. **doom_loop 历史清理** — 每次 ask 后清空（不污染下一 turn）。
129. **tests: multi-provider round-trip** — Anthropic + OpenAI + Ollama MockProvider 同一个对话跑通。
130. **tests: skill loader** — 临时目录 + sample SKILL.md，验证 frontmatter 解析 + 目录扫描。
131. **tests: doom_loop 边界** — N=3 触发，N=2 不触发，不同 args 不触发。
132. **CI: 增加 ollama smoke test** — 可选（无 Ollama 时 skip）。

### Acceptance

**GIVEN** v0.2 已完成，
**WHEN** 我配置 `[provider.openai] api_key_env="OPENAI_API_KEY" model="gpt-5"` 然后 `Ctrl+P` 切到 openai，再跑一个对话，
**THEN**
- Provider 切到 OpenAI，tool calling 协议正常，
- TUI 右侧 token budget 反映 OpenAI 模型 context window，
- 我创建 `.smallhead/skills/docker-compose/SKILL.md` 后重启 smallhead，
- `smallhead skills` 命令列出 `docker-compose` skill，
- LLM 在合适任务中自动调 `load_skill(name="docker-compose")`，SKILL.md 内容被注入后续 prompt，
- 我用 mock provider 构造 3 次连续相同 tool call → 第 4 次 LLM 想再调时被 TUI 弹窗 "doom_loop detected"。

---

## v0.4 — Durable + Context Epoch + Web + Sandbox（4 周）

**目标**：启用 durable layer opt-in + Context Epoch tracking + axum web server + 多档 sandbox。

### Tasks

133. **`--durable` CLI flag** — `smallhead run --durable "..."` 启用 DurableSession。
134. **`DurableSession` struct** — ARCHITECTURE §8.1：`inbox/history/checkpoints` 三个目录。
135. **`SessionInput` enum** — `Prompt`/`SwitchModel`/`Cancel`/`Resume` + `Delivery` enum（Steer/FollowUp/Interrupt）。
136. **`DurableSession::commit_atomic`** — 写 tmp → sync_all → rename → 同步 SQLite index。
137. **admission inbox 目录** — `~/.local/share/smallhead/inbox/{session-id}/`，inbox 不可变。
138. **`DurableSession::resume`** — 启动时检查 JSONL，标记上次遗留 running 状态为 `Error { kind: "interrupted" }`。
139. **`DurableSession` 与 `SessionStore` 桥接** — `AgentLoop` 在 `--durable` 模式下用 `DurableSession::commit_atomic` 代替 `SessionStore::append`。
140. **Crash test 1: 中途 kill -9** — 跑 agent → kill → 重启 `smallhead resume`，验证 session 续跑且 entries.jsonl 一致。
141. **`Provider::cache_key` 实现齐** — Anthropic/OpenAI/Ollama/Google 各自 cache_key 算法。
142. **`epoch_id` tracking** — `AgentLoop` 维护当前 epoch；每次 `transform_context` 前检查 epoch 变化。
143. **epoch 边界事件** — append `EntryKind::Compaction` 或 session move 时 epoch_id++。
144. **TUI epoch 可视化** — 右侧 panel 显示 `Cache: ✓ Epoch N`。
145. **`smallhead-server` 包结构** — `src/{router,handlers,ws}.rs`，`bin = "smallhead-server"`（可选独立）。
146. **axum router** — `GET /sessions`、`GET /sessions/{id}/entries`、`POST /sessions/{id}/prompt`、`GET /sessions/{id}/stream`（SSE）。
147. **WebSocket handler** — `/ws/sessions/{id}`：实时把 entries push 给前端。
148. **bind `127.0.0.1:8787` 默认** — 不暴露外网。
149. **`--web-auth-token` flag** — 设了才暴露 `0.0.0.0`。
150. **`smallhead-web` 静态资源** — `index.html` + `app.css` + `app.js`（HTMX 1 行 wire-up）。
151. **HTMX SSE 订阅** — `hx-ext="sse" sse-connect="/sessions/{id}/stream"`。
152. **Web UI: session list** — `GET /sessions` 渲染列表。
153. **Web UI: session view** — 实时 entries 流 + 简单 prompt 提交表单。
154. **Web UI: world state 视图** — `GET /sessions/{id}/world` JSON。
155. **`smallhead serve` 子命令** — 启动 server，绑定 port。
156. **`--sandbox` CLI flag** — `none/landlock/bubblewrap/docker/sandbox-exec` 5 档。
157. **`landlock` 后端** — `landlock` crate + `RulesetBuilder`，限制 fs + net。
158. **`bubblewrap` 后端** — `bubblewrap` crate（`bwrap --bind ... --ro-bind ...`）。
159. **`docker` 后端** — `--docker-image` + `--docker-network=none` flag。
160. **`sandbox-exec` 后端（macOS）** — 调 `sandbox-exec -f profile.sb <cmd>`。
161. **Bash tool subprocess sandbox** — 即使 `sandbox=none`，bash 子进程也走 `Command::new` + 白名单环境变量 + max_duration。
162. **tree-sitter bash 解析** — 解析 mutating commands（`rm`/`mv`/`>`/`dd`），自动提升为 ask。
163. **WebFetch allowed_domains** — `[permission.rules]` 已有，加白名单生效。
164. **集成测试: durable crash recovery** — spawn agent + kill + resume → state 一致。
165. **集成测试: web UI SSE** — `curl /sessions/{id}/stream` 收到实时事件。
166. **集成测试: landlock sandbox** — 在受限 fs 下跑 Bash tool，期望 write 失败。

### Acceptance

**GIVEN** v0.3 已完成，
**WHEN** 我 `smallhead run --durable --sandbox=landlock "..."` 然后中途 kill -9，再 `smallhead resume <sid>`，
**THEN**
- Agent 续跑，从上次 interrupted entry 接着，entries.jsonl 完整无丢失，
- SQLite 索引与 JSONL 一致，
- 我 `smallhead serve` 然后浏览器开 `http://localhost:8787`，看到 session list，点击 session 看到实时 entries 流（SSE），
- 我提交 prompt 后，agent 在 sandbox 里跑（landlock 限制 fs writes 仅限 cwd），
- TUI 右侧 `Cache: ✓ Epoch N` 在 compact 之后从 Epoch 1 → 2。

---

## v1.0 — Production-ready（4 周）

**目标**：完整测试套件 + 文档 + 1.0 发布。

### Tasks

167. **完整单元测试覆盖** — `cargo tarpaulin` ≥ 80% 覆盖率。
168. **完整集成测试套件** — 所有 spec 文档对应至少 1 个 integration test。
169. **可重复验证工程测试** — ARCHITECTURE §13.2：`compaction_round_trip_preserves_anchors` + `ecs_checkpoint_consistent_after_replay` 两个核心 round-trip 测试。
170. **Mock provider 覆盖所有 ProviderEvent** — 包括 `Error { retryable: true }`。
171. **doom_loop fuzz 测试** — `proptest` 或 `cargo-fuzz` 跑 1000 个随机 (tool, args) 序列。
172. **permission 规则 fuzz** — 随机生成 ruleset + 随机 input，验证 evaluate 不 panic。
173. **session 损坏恢复测试** — 故意截断 JSONL 中间 + 启动，期望 `read_all` 返回到损坏前。
174. **JSONL 损坏检测** — 启动时遍历每行 `serde_json::from_str`，记录 corrupt 行号。
175. **`.jsonl.bak` 自动备份** — 每次启动 cp `entries.jsonl` → `entries.jsonl.bak`。
176. **specs/ 目录** — 写完 `01-message-model.md` 到 `10-cli-and-tui.md`（ARCHITECTURE §2）。
177. **examples/hello-tool** — 完整可跑 example：自定义 ToolKind + 注册到 AgentLoop。
178. **examples/skill-docker** — sample SKILL.md example。
179. **README 完整化** — 安装、所有 CLI 命令、所有 config 选项。
180. **CHANGELOG.md** — v0.1 ~ v1.0 完整变更日志。
181. **CONTRIBUTING.md** — 开发环境、测试、PR 流程。
182. **docs.rs 配置** — 所有 `pub` item 有 doc comment；CI 检查 `cargo doc --no-deps -- -D warnings`。
183. **rustdoc 全公开** — `cargo doc --workspace` 无 warning。
184. **API stability 标记** — `#[deprecated]` 标注未稳定 API。
185. **MSRV 测试** — CI matrix 测 1.85 / 1.86 / 1.87。
186. **`aarch64-linux` + `x86_64-linux` + `aarch64-darwin` + `x86_64-darwin`** — 4 平台 CI matrix。
187. **`cargo audit`** — 无 high/critical 漏洞。
188. **`cargo deny`** — license/duplicate 检查通过。
189. **性能预算基准** — `criterion` 测 JSONL append、permission evaluate、compactor cold start。
190. **release build profiling** — `cargo flamegraph` 验证启动 < 100ms（ARCHITECTURE §16）。
191. **1.0 announcement 草稿** — blog post + Twitter thread + HN post。
192. **`v1.0.0` git tag** — 走 `cargo release`。
193. **crates.io publish** — `smallhead-core` 先发（依赖最少），按依赖序发其余。

### Acceptance

**GIVEN** v0.4 已完成且所有 CI 通过，
**WHEN** 我跑 `cargo test --workspace`，
**THEN**
- 所有单元测试 + 集成测试 pass（≥ 200 个 test），
- `cargo tarpaulin` 显示 ≥ 80% line coverage，
- `cargo doc --no-deps -- -D warnings` 无 warning，
- `cargo audit` 无 high/critical 漏洞，
- 性能预算：JSONL append < 5ms（P95）、permission evaluate < 1ms（P95）、compactor cold start < 5s（10MB session P95）、
- `cargo build --release` 在 4 个 target 平台（linux/macos x86_64+aarch64）都成功，
- `smallhead-cli` 和 `smallhead-server` 都发布到 crates.io，
- GitHub release tag `v1.0.0` + 自动生成 changelog。

---

## 总结

| 版本 | 周数 | 累计 | 关键里程碑 |
|------|------|------|----------|
| v0.1 | 6 | 6 | 单 turn + 5 tools + Anthropic |
| v0.2 | 4 | 10 | ECS + TUI + Compaction |
| v0.3 | 4 | 14 | 多 provider + Skill |
| v0.4 | 4 | 18 | Durable + Web + Sandbox |
| v1.0 | 4 | 22 | 完整测试 + 1.0 发布 |
