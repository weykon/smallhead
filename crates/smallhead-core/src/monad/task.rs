//! Monad task 生命周期管理 —— 把"单次消费 / 重绑检测 / 完成清理"统一收口。
//!
//! 本轮只做"局部绑定"(`TaskSlot` + 配套 `TaskBinding`),不做 TaskManager /
//! observer / 锁 / 重试占位字段,也不引入 typestate。未来跨实例状态管理
//! 仍由调度器负责,见 `docs/monad-orchestration-pattern.md` 与
//! `docs/task-slot-refactor-design.md` §14.3。
//!
//! ## 类型
//!
//! - [`TaskBinding`][]:已通过校验的标识与本地计时数据;**只**由
//!   [`TaskBinding::from_handoff`] 构造,业务代码不应直接拼装字段。
//! - [`TaskSlot`]:每个 monad 实例持有一个,守 `try_bind` / `take` / `clear` 三个门。
//!
//! ## 状态机
//!
//! ```text
//!         ┌──────────┐  try_bind(h)   ┌──────────┐
//!         │ Unbound  │ ──────────────▶│  Bound   │
//!         │ (None)   │                │ (Some b) │
//!         └──────────┘                └──────┬───┘
//!               ▲                            │
//!               │  take() / clear()          │
//!               └────────────────────────────┘
//! ```
//!
//! ## 与持久化的关系
//!
//! `TaskBinding::bound_at` 使用 [`std::time::Instant`](std::time::Instant) 来度量
//! 本进程内的单调时钟,只为 [`TaskSlot::usage_after`] 算 `elapsed_ms`。
//! 跨进程恢复时不要持久化这个字段 —— `MonadResult::finished_at_ms` 仍然使用
//! epoch 毫秒,跨进程可比。

use std::time::Instant;

use crate::{
    MonadError, MonadHandoff, MonadId, ResourceUsage, ResultSlot, TaskId, SCHEMA_VERSION_V1,
};

/// 已通过校验的任务绑定。
///
/// 字段私有，只能通过 [`TaskBinding::from_handoff`] 校验构造。
/// 对外提供只读查询，Monad 内部通过消费方法搬移回填标识。
/// 包含本地计时数据，不作为跨进程持久化记录。
///
/// 外部代码不能绕过校验直接构造：
/// ```compile_fail
/// use smallhead_core::monad::TaskBinding;
/// use smallhead_core::{TaskId, ResultSlot};
/// let binding = TaskBinding {
///     task_id: TaskId(String::new()),
///     parent_task_id: None,
///     result_slot: ResultSlot(String::new()),
///     bound_at: std::time::Instant::now(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct TaskBinding {
    /// 本次 handoff 的 task_id(uuid-v7)。
    task_id: TaskId,
    /// 父 task 的 id(顶层任务时为 `None`)。
    parent_task_id: Option<TaskId>,
    /// 回填槽位。
    result_slot: ResultSlot,
    /// `try_bind` 接收 handoff 的本地时刻,仅用于 `usage_after` 算 elapsed。
    /// 不可跨进程持久化。
    bound_at: Instant,
}

impl TaskBinding {
    /// 6 项校验,沿用原 `monad.rs::TaskBinding::from_handoff` 的规则:
    ///
    /// 1. `monad_id` 与 `expected` 匹配;
    /// 2. `schema_version == SCHEMA_VERSION_V1`;
    /// 3. `task_id` 非空白;
    /// 4. `result_slot` 非空白;
    /// 5. `parent_task_id` 若有则非空白;
    /// 6. `attempt_id > 0`。
    ///
    /// 任何一项不过都返回 [`MonadError`],原 monad 实例与业务字段保持不变。
    pub fn from_handoff(handoff: &MonadHandoff, expected: &MonadId) -> Result<Self, MonadError> {
        if &handoff.monad_id != expected {
            return Err(MonadError::new("handoff targets a different monad"));
        }
        if handoff.schema_version != SCHEMA_VERSION_V1 {
            return Err(MonadError::new("unsupported handoff schema_version"));
        }
        if handoff.task_id.0.trim().is_empty()
            || handoff.result_slot.0.trim().is_empty()
            || handoff
                .parent_task_id
                .as_ref()
                .is_some_and(|id| id.0.trim().is_empty())
            || handoff.attempt_id == 0
        {
            return Err(MonadError::new(
                "handoff requires nonempty IDs/result_slot and a positive attempt_id",
            ));
        }
        Ok(Self {
            task_id: handoff.task_id.clone(),
            parent_task_id: handoff.parent_task_id.clone(),
            result_slot: handoff.result_slot.clone(),
            bound_at: Instant::now(),
        })
    }

    /// 本次任务的标识。
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// 父任务标识；顶层任务没有父任务。
    pub fn parent_task_id(&self) -> Option<&TaskId> {
        self.parent_task_id.as_ref()
    }

    /// 结果回填槽位。
    pub fn result_slot(&self) -> &ResultSlot {
        &self.result_slot
    }

    /// 单次消费标识，供父模块构造结果包。
    pub(super) fn into_parts(self) -> (TaskId, Option<TaskId>, ResultSlot) {
        (self.task_id, self.parent_task_id, self.result_slot)
    }
}

/// 一个 monad 实例的 task 状态守门。
///
/// 替代散装的 `task: Option<TaskBinding>` + `started_ms: i64` + 5 行 `is_some()` 判重。
/// `take` 语义 = "执行权已移交",**不是** "任务已完成";等待上下文的 monad 在
/// `take` 之后仍可由上层保留任务身份,然后再次 `try_bind` 补充后的 handoff。
#[derive(Debug, Default)]
pub struct TaskSlot {
    inner: Option<TaskBinding>,
}

impl TaskSlot {
    /// 构造一个空 slot。
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已绑定。
    pub fn is_bound(&self) -> bool {
        self.inner.is_some()
    }

    /// 读当前 binding(不消费)。主要用于调试、日志、构造 result_slot 派生字段。
    pub fn peek(&self) -> Option<&TaskBinding> {
        self.inner.as_ref()
    }

    /// 尝试绑定一个新 handoff。
    ///
    /// - 校验 [`TaskBinding::from_handoff`] 6 项规则,任一不过返回 `Err`;
    /// - 已绑定时拒绝,`Err` 信息 `"monad already has an active task"`,**不**
    ///   覆盖原 binding / 业务输入 / 消息 buffer;
    /// - 成功时返回新 binding 的不可变引用。
    ///
    /// **业务输入准备的顺序**:调用方应先在本地变量准备好业务输入,确认
    /// `try_bind` 成功之后,再写入 monad 实例的字段;若 `try_bind` 失败,
    /// monad 状态应**完全不变**(包括 `expression` / `name` / `text` /
    /// `frame` / `started` 等)。
    pub fn try_bind(
        &mut self,
        handoff: &MonadHandoff,
        expected: &MonadId,
    ) -> Result<&TaskBinding, MonadError> {
        if self.inner.is_some() {
            return Err(MonadError::new("monad already has an active task"));
        }
        let binding = TaskBinding::from_handoff(handoff, expected)?;
        self.inner = Some(binding);
        Ok(self.inner.as_ref().expect("just inserted"))
    }

    /// 单次消费 binding。`step()` 入口处调用,**不** 表示任务已完成,只表示
    /// "执行权已交给本步"。
    ///
    /// 未绑定时返回 `Err`,信息 `"no active task; call start(handoff) before step()"`。
    pub fn take(&mut self) -> Result<TaskBinding, MonadError> {
        self.inner
            .take()
            .ok_or_else(|| MonadError::new("no active task; call start(handoff) before step()"))
    }

    /// 清除当前 binding。`cancel()` 入口处调用;重复调用是 no-op。
    pub fn clear(&mut self) {
        self.inner = None;
    }

    /// 由 binding 的 `bound_at` 与当前时刻的差,加上调用方传入的 steps / tokens,
    /// 拼成 [`ResourceUsage`],作为 `MonadResult::Completed.usage` 使用。
    ///
    /// 设计上 [`Instant`] 只用于本进程耗时;`finished_at_ms` 继续走 epoch 毫秒。
    pub fn usage_after(binding: &TaskBinding, steps_taken: u32, tokens_used: u32) -> ResourceUsage {
        ResourceUsage {
            steps_taken,
            tokens_used,
            elapsed_ms: binding.bound_at.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    //! 集中守住 §14.2 表中"TaskSlot 状态机"契约。
    //!
    //! 1. try_bind 成功 → 后续重复 try_bind 必须失败,原 binding 不可变;
    //! 2. take() 未绑定时返回 Err;
    //! 3. take() 后 slot 重新可绑;
    //! 4. clear() 幂等;
    //! 5. peek 不消费;
    //! 6. `TaskBinding::from_handoff` 6 项校验独立测过一遍,这里只验组合;
    //! 7. 校验失败时 slot 保持原状态(不会出现"半绑"残留);
    //! 8. usage_after 包含 bound_at 到 now 的 elapsed 毫秒。
    use super::*;
    use crate::{Budget, MonadHandoff, TraceMeta};

    fn handoff(monad: &str) -> MonadHandoff {
        MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.into(),
            task_id: TaskId::new(),
            parent_task_id: Some(TaskId::new()),
            monad_id: MonadId::new(monad),
            attempt_id: 1,
            objective: "test".into(),
            inputs: serde_json::Map::new(),
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "ok".into(),
            result_slot: ResultSlot("slot".into()),
            depends_on: vec![],
            budget: Budget::default(),
            issued_at_ms: 0,
            trace: TraceMeta {
                frame_ref: "f".into(),
                request_id: "r".into(),
            },
        }
    }

    #[test]
    fn new_slot_is_empty() {
        let slot = TaskSlot::new();
        assert!(!slot.is_bound());
        assert!(slot.peek().is_none());
    }

    #[test]
    fn try_bind_then_double_bind_keeps_original() {
        let mut slot = TaskSlot::new();
        let h1 = handoff("echo");
        slot.try_bind(&h1, &MonadId::new("echo")).unwrap();
        let original = slot.peek().cloned().expect("just bound");

        let h2 = handoff("echo");
        let err = slot
            .try_bind(&h2, &MonadId::new("echo"))
            .expect_err("second bind must fail");
        assert!(
            err.0.contains("monad already has an active task"),
            "msg: {}",
            err.0
        );

        // 原 binding 未变
        let still = slot.peek().expect("binding must remain");
        assert_eq!(still.task_id, original.task_id);
        assert_eq!(still.result_slot, original.result_slot);
        assert_eq!(still.parent_task_id, original.parent_task_id);
    }

    #[test]
    fn failed_try_bind_leaves_slot_untouched() {
        let mut slot = TaskSlot::new();
        let bad = {
            let mut h = handoff("echo");
            h.monad_id = MonadId::new("wrong-monad");
            h
        };
        let err = slot
            .try_bind(&bad, &MonadId::new("echo"))
            .expect_err("monad_id mismatch must reject");
        assert!(err.0.contains("handoff targets a different monad"));
        assert!(!slot.is_bound(), "rejected bind must not leave state");
        assert!(slot.peek().is_none());

        // 之后的合法 bind 必须成功
        let good = handoff("echo");
        slot.try_bind(&good, &MonadId::new("echo"))
            .expect("post-rejection bind ok");
        assert!(slot.is_bound());
    }

    #[test]
    fn take_consumes_and_releases() {
        let mut slot = TaskSlot::new();
        let h = handoff("echo");
        slot.try_bind(&h, &MonadId::new("echo")).unwrap();
        let taken = slot.take().expect("take must succeed while bound");
        assert_eq!(taken.task_id, h.task_id);
        assert!(!slot.is_bound(), "take must release binding");
        // 二次 take → Err
        let err = slot.take().expect_err("second take must Err");
        assert!(err.0.contains("no active task"));
        // 再次 try_bind 应通过
        let h2 = handoff("echo");
        slot.try_bind(&h2, &MonadId::new("echo"))
            .expect("re-bind after take ok");
    }

    #[test]
    fn take_on_empty_returns_err() {
        let mut slot = TaskSlot::new();
        let err = slot.take().expect_err("take on empty must Err");
        assert!(err.0.contains("no active task"));
        assert!(!slot.is_bound());
    }

    #[test]
    fn clear_is_idempotent() {
        let mut slot = TaskSlot::new();
        slot.clear();
        assert!(!slot.is_bound());
        let h = handoff("echo");
        slot.try_bind(&h, &MonadId::new("echo")).unwrap();
        slot.clear();
        assert!(!slot.is_bound());
        slot.clear(); // 二次 clear no-op
        assert!(!slot.is_bound());
    }

    #[test]
    fn peek_does_not_consume() {
        let mut slot = TaskSlot::new();
        let h = handoff("echo");
        slot.try_bind(&h, &MonadId::new("echo")).unwrap();
        let p1 = slot.peek().expect("peek while bound");
        let p2 = slot.peek().expect("peek still");
        assert_eq!(p1.task_id, p2.task_id);
        assert!(slot.is_bound());
    }

    #[test]
    fn from_handoff_rejects_each_validation_path() {
        type InvalidCase = (&'static str, fn(&mut MonadHandoff));
        // 6 项校验逐项独立失败
        let expected = MonadId::new("echo");
        let make = |patch: fn(&mut MonadHandoff)| {
            let mut h = handoff("echo");
            patch(&mut h);
            h
        };
        let cases: [InvalidCase; 6] = [
            ("monad_mismatch", |h| h.monad_id = MonadId::new("other")),
            ("bad_schema", |h| h.schema_version = "v0".into()),
            ("empty_task", |h| h.task_id = TaskId("  ".into())),
            ("empty_result_slot", |h| {
                h.result_slot = ResultSlot(String::new())
            }),
            ("empty_parent", |h| {
                h.parent_task_id = Some(TaskId(String::new()))
            }),
            ("zero_attempt", |h| h.attempt_id = 0),
        ];
        for (name, patch) in cases.iter() {
            let h = make(*patch);
            let _err = TaskBinding::from_handoff(&h, &expected)
                .expect_err_with(|| format!("{name}: must reject"));
        }
    }

    #[test]
    fn usage_after_measures_elapsed() {
        let h = handoff("echo");
        let binding = TaskBinding::from_handoff(&h, &MonadId::new("echo")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let usage = TaskSlot::usage_after(&binding, 3, 17);
        assert_eq!(usage.steps_taken, 3);
        assert_eq!(usage.tokens_used, 17);
        assert!(
            usage.elapsed_ms >= 2,
            "elapsed should reflect at least the sleep"
        );
    }

    /// 期望 Result 是 Err,返回 Err 值;是 Ok 时 panic 并附上 `f` 拼出的 case 名。
    trait ExpectErrWith<T, E> {
        fn expect_err_with(self, f: impl FnOnce() -> String) -> E
        where
            E: std::fmt::Debug,
            T: std::fmt::Debug;
    }
    impl<T: std::fmt::Debug, E: std::fmt::Debug> ExpectErrWith<T, E> for Result<T, E> {
        fn expect_err_with(self, f: impl FnOnce() -> String) -> E {
            match self {
                Err(e) => e,
                Ok(v) => panic!("{}: expected Err, got Ok({v:?})", f()),
            }
        }
    }
}
