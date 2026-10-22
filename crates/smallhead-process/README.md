# smallhead-process

`smallhead-process` 是 small-head 中 OS 命令和常驻服务的唯一生命周期管理层。它持有子进程、执行取消与超时、保存审计日志，并向 Agent 与 TUI 提供同一份状态事实。

## 当前完成度（2026-10-10）

| 能力 | 状态 | 已实现 | 尚未实现 |
|---|---|---|---|
| S1 进程生命周期 | 可用（Unix/macOS） | 九状态状态机；真实退出码；超时、取消、TERM→KILL；Unix 进程组清理；重复 stop；调用方 future 被丢弃后仍可查询/停止；显式 shutdown | Windows Job Objects；主动脱离进程组的后代隔离；管理器被 OS 强杀后的完整清理证明 |
| S2 输出与资源 | 可用基础版 | stdout/stderr 流式落盘；每流字节上限；输出超限停止；并发、活跃数、记录数、总磁盘预算；分页读取；显式 purge；恢复执行记录 | 自动保留/GC；head/tail/error 摘要文件；CPU、内存、进程数硬限制 |
| S3 服务 | 可用基础版 | `ensure/inspect/stop/forget`；名称身份；readiness JSON 验证；并发 ensure 去重；外部服务只复用不停止；Laya/Qwen 预设；`services.json` 持久登记与保守恢复 | 自动重启与退避预算；服务消费者/依赖传播；跨应用持久 supervisor |
| S4 Agent/TUI | 已接通 | `BashTool` 薄适配；`ProcessTool`；execution_id；F3 进程页、F4 停止、输出和服务状态；结构化事件；`completion_policy.type=exited` 的允许退出码判断 | PTY/交互 stdin；artifact/ready 等声明式业务完成条件；完整 TaskGraph 依赖事件 |

这里的“可用”不是“所有平台生产隔离完成”。Unix/macOS 是当前亲自验证目标；平台边界和剩余能力见下文。

## 快速使用

```rust
use smallhead_process::{ProcessManager, ProcessSpec};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = ProcessManager::new()?;
    let id = manager
        .start(ProcessSpec::program("printf", vec!["hello\\n".into()]))
        .await?;
    let record = manager.wait_for(id, Duration::from_secs(5)).await?;
    println!("{id}: {:?}", record.state);
    manager.shutdown(Duration::from_secs(6)).await?;
    Ok(())
}
```

默认日志根目录是 `$HOME/.small-head/runs`。测试和 demo 使用 `ProcessManager::with_log_root(path)` 隔离目录。一个日志根目录同一时刻只能由一个 manager 持有；锁定失败会拒绝创建第二个 manager。

## 主要接口

| 类型/接口 | 含义 |
|---|---|
| `ExecutionId` | 一次执行尝试的 UUID v7；拒绝和 spawn 失败也有 ID |
| `ProcessId` | OS PID，与 ExecutionId 分开 |
| `ProcessSpec::new` | 显式 shell 模式，经 `sh -c` 执行 |
| `ProcessSpec::program` | 直接 program + args，保留参数边界 |
| `ProcessManager::start/start_owned` | 登记、校验并启动；返回 ID 不代表命令已经成功 |
| `inspect/list/wait_for/stop` | 查询、等待和幂等停止 |
| `read_output` | 按字节 offset 分页读取 stdout、stderr 或 events，单次最多 64 KiB |
| `purge` | 显式删除已确认完成的记录和日志 |
| `shutdown` | 关闭 admission，停止并等待所有受管执行收尾 |
| `ServiceManager` | 在 ProcessManager 上管理常驻服务的 readiness 与所有权 |

`ProcessLimits` 默认值：并发子进程 4、活跃执行 32、记录 1024、日志根预算 512 MiB、每流 4 MiB、默认执行时限 120 秒、最大 600 秒。排队时间不计入命令执行时限。超出单流预算会停止进程组并记录 `ExitReason::OutputLimit`。

## 磁盘布局与恢复

```text
<log_root>/
├── .manager.lock
├── services.json
└── <execution_id>/
    ├── stdout.log
    ├── stderr.log
    ├── events.jsonl
    └── meta.json
```

`meta.json` 和 `services.json` 通过临时文件后 rename 更新。环境变量不写入序列化记录。重启时，未终结的执行被标为 `Lost`，系统绝不会仅凭旧 PID 发送信号。

服务恢复使用持久化执行证据：仍期望运行、且关联的同名服务执行已经明确退出（或拒绝、启动失败）的登记恢复为 `Failed`，可再次显式 `ensure`，保留原配置和日志。缺少证据或执行为 `Lost` / `CleanupFailed` 时仍为 `Unknown`，不按旧 PID 操作。后续 `ensure` 若通过身份健康协议，会把未知实例视为外部服务复用，因此没有停止权。未知且验证失败时需要人工核对；清除服务登记不等于消除了历史执行的不确定性。

## Agent 与 TUI 接入

`smallhead-core::BashTool` 不再自行持有 `tokio::process::Child`，而是将输入转换成 `ProcessSpec`。`process_control` 提供执行的 list/inspect/stop/read_output/purge，以及服务的 list/ensure/inspect/stop/forget。TUI 的 F3 页面和 Agent 工具读取同一个 manager。BashTool 会持久化 `objective` 和 `completion_policy`；当前已支持 `type=exited` 搭配 `allowed_exit_codes` 影响工具成功判断。

有限命令默认绑定 task owner；任务结束时发出停止请求。`SessionService` 只由 `ServiceManager` 内部启动，跨聊天轮次保留，并在应用正常 shutdown 时清理。`PersistentService` 仍不支持。

## 验证

```sh
cargo run -p smallhead-process --example s1_demo
cargo run -p smallhead-process --example resource_demo
cargo test -p smallhead-process
cargo test -p smallhead-core --test execution_integration
cargo test -p smallhead-tui
```

本地伪 HTTP 服务测试覆盖服务复用、错误身份、并发 ensure、启动超时、显式停止、登记恢复与安全 forget。真实 Laya/Qwen 还要求本机模型和 Python 环境可用；它们未运行时，连接拒绝不是 Rust 路由成功的证据。

## 保证边界

- Unix 使用独立进程组并执行 TERM、有限 grace、KILL。主动 `setsid` 脱离该组的后代不在保证范围。非 Unix 当前只保证直接子进程清理。
- 输出日志保存到每流预算上限；当前保存前缀并以 `OutputLimit` 结束执行，没有无限“完整输出”承诺。
- 总磁盘预算在启动前预留，但没有自动 GC；达到记录或磁盘上限后需显式 purge 已完成执行。
- permission 是执行边界的启发式规则，不是 shell 沙箱。结构化 program + args 能避免不必要的 shell 展开，但不能替代 OS 隔离。
- CPU、内存和后代数量还没有硬限制；不能把采样或超时描述成这些资源的硬配额。
- 应用必须在关闭 Tokio runtime 前调用 `shutdown`。`kill_on_drop` 只是直接子进程兜底，不等价于崩溃恢复。

状态只能由 manager/executor 转换。最终元数据会先落盘，再向等待者发布终态；审计失败会成为 `CleanupFailed`，不会伪造成功退出。
