# Monad 的 Rust 生命周期与业务标识修复

2026-10-07；实现位于 `crates/smallhead-core/src/monad.rs`。

## 本次发现与修复

- 三个 fixture 的 start 已填 task_id，但 Completed 都把 parent_task_id 写成 None，导致父子任务关联丢失。现完整保留交接标识。
- 工厂通过 empty_handoff 生成假 task_id 和 default 槽位，未 start 也可能产出结果。已删除此路径，factory 返回 Default 未绑定实例。
- Echo 可重复 Completed，取消没有阻止后续 step。现在一次 step 消费绑定，取消清除绑定。
- Main 连续 start 会覆盖任务标识却追加消息，造成旧输入配上新任务 ID。现在拒绝覆盖活跃任务，校验成功后才写状态。
- Main 完成后 started 未复位，取消后本地 frame 和任务标识未清除。现在同步清理。

## Option 的业务含义

内部使用 `Option<TaskBinding>`，而不是为每个必填 ID 分别设置 Option：

```rust
struct TaskBinding {
    task_id: TaskId,
    parent_task_id: Option<TaskId>,
    result_slot: ResultSlot,
}
```

外层 None 表示没有可执行的任务绑定；Some 表示必填标识同时存在。内层 parent_task_id=None 是合法的根任务，不表示漏填。两种 None 的业务语义不同。

边界验证 monad 目标、schema_version、非空 task_id/result_slot、非空可选 parent ID、正数 attempt_id。newtype 防止 Rust 中混传类型，但 public String 和 JSON 仍可能携带无效内容，编译通过不代表业务有效。本次非空校验不等于已验证 UUID 格式、任务存在性或父子依赖合法性。

fixture 的 step 用 take 消费绑定，防止同一绑定重复产生结果。这不是跨重启或外部系统的 exactly-once 保证。

## 开发接口变化

- `instantiate(id)` 返回独立未绑定实例，必须先 `start(handoff).await?` 再 `step().await?`。
- `EchoMonad/CalculationMonad/GreetingMonad::from_handoff(&handoff)` 现在返回 `Result<Self, MonadError>`，可用 `?` 构造已绑定实例，不再额外调用 start。
- 活跃实例再次 start 返回错误；完成或取消后可接新任务。
- Calculation 返回 WaitingContext 后已消费本次绑定，补充输入后重新 start 对应 handoff，再 step。未来持久化 scheduler 应显式保存等待状态、任务版本与恢复语义。
- 工厂保留 `Fn`，职责只是每次构造独立实例；没有必要为此另加工厂 trait。

## 测试与范围

`env -u RUSTUP_TOOLCHAIN cargo test -p smallhead-core --lib --quiet`：78/78 通过；新增 6 项生命周期回归测试。只格式化 monad.rs，未修改其他 agent 的 Cargo、memory、tool 等工作。

仍需后续设计：Main provider 失败后的重试与副作用去重、运行中取消与可恢复调度、多 turn/steering、严格业务输入 schema。当前输入默认值和算术解析器并非完整业务验证器。本次未将这些扩展混入标识修复。

原说明中的“core crate zero-dep”不准确：core 已依赖 tokio、serde 等；使用 std::time 只是没有为时间功能新增 chrono 依赖。snake_case 只约定字段和枚举标签，跨语言端仍需匹配可选字段及具体数据类型。
