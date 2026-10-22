# Smallhead

Smallhead 是用 Rust 编写的 CLI / TUI agent 运行底座：统一的 ReAct loop、可组合 Lens 策略、Monad 子 agent 协作，以及 ECS World 和持久化对话。

项目仍在持续开发，接口和包格式可能变化。与 [Bighead](https://github.com/weykon/bighead) 共享 ECS 基础；Bighead 更侧重可编程 world、Rhai 和桌面 / 空间界面，Smallhead 侧重终端执行与可组合的 agent loop。

## 快速开始

当前 ECS crate 通过相邻目录引用 Bighead 的 vendored Bevy，因此请保留下面两个本地目录名：

```sh
git clone https://github.com/weykon/bighead.git big-head
git clone https://github.com/weykon/smallhead.git small-head
cd small-head

# 安装 rust-toolchain.toml 指定的工具链
rustup toolchain install 1.99.0 --profile minimal --component rustfmt --component clippy

# 离线界面演示；固定脚本不理解真实问题
cargo +1.99.0 run -p smallhead-tui -- --mock
```

真实聊天使用 MiniMax，先设置环境变量：

```sh
export MINIMAX_API_KEY='your-api-key'
cargo +1.99.0 run -p smallhead-tui
```

也可使用根目录的 `.env.example` 配置本地 `.env`。不要提交真实凭据。完整 TUI 的依赖包含 LanceDB，构建开销较大；当前以 macOS / Unix 开发验证为主。

## 当前能力

- AgentLoop：流式模型调用、工具执行、运行中补充信息、取消与错误诊断。
- 上下文：大文本 artifact 投影、rolling history compaction、持久化对话与恢复。
- Monad：委派、汇报、双向消息、暂停 / 继续，以及独立任务上下文。
- Lens：Rust 策略与进程外 JavaScript worker，组合 system、工具视野、调用处理、继续和结束策略；可发现、加载、重载、卸载。
- 工具：文件与命令执行、Web 搜索、Agent Reach CLI、MCP、项目研究与 ECS world 操作。
- 进程与服务：受管执行、输出预算、服务持久化与恢复、健康检查及 TUI 服务页。
- TUI：Markdown 助手消息、日志、用户选择、计划进度、方案阅读与评论。

功能边界和具体键位以各 crate 文档及实际实现为准；旧设计稿可能包含尚未实现的目标。没有包含模型权重、个人记忆库或教材。仓库内 `lenses/learning` 是示例策略；完整 Learning Agent 领域 worker 仍在独立项目中，并不随此仓库发布。

## Lens

JavaScript Lens 需要本机可用的 Node.js。TUI 中可加载仓库示例：

```text
/lens load lenses/evidence-check
/lens list
```

也可以用自然语言请求 agent 发现和管理本机 Lens。配置、扩展工具、生命周期与限制见 [Lens 文档](crates/smallhead-lens/README.md)。

## 代码导航

| 模块 | 职责 |
| --- | --- |
| `smallhead-core` | AgentLoop、模型协议、上下文管理、Monad 与 loop policy |
| `smallhead-lens` | Lens 包、JS worker、扩展工具与管理 |
| `smallhead-tools` | 搜索、Agent Reach 与内置工具装配 |
| `smallhead-mcp` | MCP 配置、连接与工具桥接 |
| `smallhead-process` | 进程执行、存储与 ServiceManager |
| `smallhead-session` | 对话与事件持久化 |
| `smallhead-ecs` | ECS world 操作与 checkpoint |
| `smallhead-memory` | 资料切分、embedding 与向量存储 |
| `smallhead-permission` | 权限与执行约束 |
| `smallhead-skill` | Skill 模块，具体完成度见实现 |
| `smallhead-xerj` | 项目研究业务适配 |
| `smallhead-cli` | CLI 宿主与公共装配 |
| `smallhead-server` | HTTP / A2A 宿主 |
| `smallhead-web` | Web 资源 |
| `smallhead-tui` | 终端交互界面 |

## 深入了解

- [TUI 用法](crates/smallhead-tui/README.md)
- [Lens 策略与扩展](crates/smallhead-lens/README.md)
- [进程与服务](crates/smallhead-process/README.md)
- [MCP](crates/smallhead-mcp/README.md)
- [搜索工具](crates/smallhead-tools/README.md)
- [架构](ARCHITECTURE.md) 与 [方法论](AGENT_METHODOLOGY.md)
- [Monad 协作](docs/monad-collaboration.md)
- [开源快照说明](PUBLICATION.md)

## License

Smallhead 以 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可证发布，任选其一。第三方依赖保留各自许可证。
