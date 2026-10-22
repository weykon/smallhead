//! 真 fixture monad 实现 —— 不接 LLM,纯本地计算。
//!
//! 对应 `docs/monad-orchestration-pattern.md` §2.4。
//!
//! 这些 fixture 替换了 Slice 2 原本的"全部用 MainMonad::empty() 包装"的占位实现。
//! 它们**不**接 Provider / 不接 AgentLoop —— 直接对 `handoff.inputs` 做确定性变换,
//! 让 `instantiate()` 返回的实例 `monad_id()` 真的对得上 fixture 的 monad_id。
//!
//! 这是 P1 / P3 之前能做的最简"有意义"monad。生产 monad 接 LLM 的写法
//! 等 P1 async scheduler 落地,以 [`crate::MainMonad`] 的 start/step 桥接为参考。

use crate::monad::loop_trait::{CatalogEntry, MonadError, MonadLoop};
use crate::monad::main_monad::now_ms;
use crate::monad::protocol::{MonadHandoff, MonadId, MonadResult};
use crate::monad::task::TaskSlot;
use crate::tool::ToolKind;
use async_trait::async_trait;
use std::sync::OnceLock;

/// EchoMonad:把 inputs.echo_text 原样作为 MonadResult::Completed.output 返回。
///
/// 不接 LLM,只做字符串透传。可用于联调 / prompt 注入测试。
#[derive(Default)]
pub struct EchoMonad {
    task: TaskSlot,
    text: String,
}

impl EchoMonad {
    /// 直接构造已绑定实例；无 handoff 的 factory 应使用 Default。
    pub fn from_handoff(handoff: &MonadHandoff) -> Result<Self, MonadError> {
        let mut instance = Self::default();
        instance.bind(handoff)?;
        Ok(instance)
    }

    fn bind(&mut self, handoff: &MonadHandoff) -> Result<(), MonadError> {
        // 先在局部变量准备好业务输入;只有 try_bind 成功后才写回 self。
        // 这样 handoff 校验失败时实例保持完全未绑状态。
        let text = handoff
            .inputs
            .get("echo_text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // 拿到 expected 后再调用 try_bind,避免 self 同时被借用。
        let expected = self.monad_id().clone();
        self.task.try_bind(handoff, &expected)?;
        self.text = text;
        Ok(())
    }
}

#[async_trait]
impl MonadLoop for EchoMonad {
    fn monad_id(&self) -> &MonadId {
        static ID: OnceLock<MonadId> = OnceLock::new();
        ID.get_or_init(|| MonadId::new("echo"))
    }

    fn catalog_entry(&self) -> &CatalogEntry {
        static ENTRY: OnceLock<CatalogEntry> = OnceLock::new();
        ENTRY.get_or_init(|| CatalogEntry {
            summary: "原样回显 inputs.echo_text 字段,不接 LLM".to_string(),
            visible_tools: vec![],
            keywords: vec!["echo".into(), "回显".into(), "联调".into()],
        })
    }

    fn default_tool_set(&self) -> Vec<ToolKind> {
        // 不接 LLM,不需要工具。
        vec![]
    }

    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.bind(&handoff)
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let task = self.task.take()?;
        // 单步完成:把 text 包装成 MonadResult::Completed。
        let now = now_ms();
        // 先用 &task 算出 usage,再把 task 的字段搬进 Completed —— 避免
        // 局部 move 之后再用 &task 触发 E0382。
        let usage = TaskSlot::usage_after(&task, 1, 0);
        let (task_id, parent_task_id, result_slot) = task.into_parts();
        Ok(MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            monad_id: MonadId::new("echo"),
            output: serde_json::json!({ "echo_text": self.text.clone() }),
            evidence_refs: vec![],
            usage,
            finished_at_ms: now,
        })
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task.clear();
        Ok(())
    }
}

/// CalculationMonad:解析 inputs.expression 并求值。
///
/// MVP 只支持 `+ - * /` 四则运算 + 整数。不接 LLM。
/// expression 解析失败时返回 `MonadResult::WaitingContext`(语义上合理:
/// 让主循环提示用户修正表达式)。
#[derive(Default)]
pub struct CalculationMonad {
    task: TaskSlot,
    expression: String,
}

impl CalculationMonad {
    /// 直接构造已绑定实例；无 handoff 的 factory 应使用 Default。
    pub fn from_handoff(handoff: &MonadHandoff) -> Result<Self, MonadError> {
        let mut instance = Self::default();
        instance.bind(handoff)?;
        Ok(instance)
    }

    fn bind(&mut self, handoff: &MonadHandoff) -> Result<(), MonadError> {
        // 先在局部变量准备好业务输入;只有 try_bind 成功后才写回 self。
        let expression = handoff
            .inputs
            .get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expected = self.monad_id().clone();
        self.task.try_bind(handoff, &expected)?;
        self.expression = expression;
        Ok(())
    }
    /// 极简算术表达式求值器 —— 仅支持 `整数 (op 整数)+`,op ∈ `+ - * /`。
    /// 返回 `Err(reason)` 给 WaitingContext 用。
    fn eval(expr: &str) -> Result<f64, String> {
        let cleaned: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
        if cleaned.is_empty() {
            return Err("empty expression".into());
        }
        // 顺序匹配操作符(避免 - 出现在 split 时被误判)
        for op in ['+', '-', '*', '/'] {
            if let Some(idx) = cleaned.find(op) {
                let (lhs_s, rhs_s) = cleaned.split_at(idx);
                let rhs_s = &rhs_s[1..]; // 跳过 op 字符
                let lhs: f64 = lhs_s
                    .parse()
                    .map_err(|_| format!("left operand {:?} is not a number", lhs_s))?;
                let rhs: f64 = rhs_s
                    .parse()
                    .map_err(|_| format!("right operand {:?} is not a number", rhs_s))?;
                let value = match op {
                    '+' => lhs + rhs,
                    '-' => lhs - rhs,
                    '*' => lhs * rhs,
                    '/' => {
                        if rhs == 0.0 {
                            return Err("division by zero".into());
                        }
                        lhs / rhs
                    }
                    _ => unreachable!(),
                };
                return Ok(value);
            }
        }
        Err(format!(
            "expression {:?} has no recognised operator (+/-) or * or /",
            expr
        ))
    }
}

#[async_trait]
impl MonadLoop for CalculationMonad {
    fn monad_id(&self) -> &MonadId {
        static ID: OnceLock<MonadId> = OnceLock::new();
        ID.get_or_init(|| MonadId::new("calculation"))
    }

    fn catalog_entry(&self) -> &CatalogEntry {
        static ENTRY: OnceLock<CatalogEntry> = OnceLock::new();
        ENTRY.get_or_init(|| CatalogEntry {
            summary: "解析并求值 inputs.expression 算术表达式,不接 LLM".to_string(),
            visible_tools: vec![],
            keywords: vec![
                "calculation".into(),
                "计算".into(),
                "arithmetic".into(),
                "算术".into(),
            ],
        })
    }

    fn default_tool_set(&self) -> Vec<ToolKind> {
        vec![]
    }

    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.bind(&handoff)
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let task = self.task.take()?;
        match Self::eval(&self.expression) {
            Ok(value) => {
                let usage = TaskSlot::usage_after(&task, 1, 0);
                let (task_id, parent_task_id, result_slot) = task.into_parts();
                Ok(MonadResult::Completed {
                    task_id,
                    parent_task_id,
                    result_slot,
                    monad_id: MonadId::new("calculation"),
                    output: serde_json::json!({
                        "expression": self.expression.clone(),
                        "value": value,
                    }),
                    evidence_refs: vec![],
                    usage,
                    finished_at_ms: now_ms(),
                })
            }
            Err(reason) => Ok(MonadResult::WaitingContext {
                task_id: task.task_id().clone(),
                reason: reason.clone(),
                missing: vec!["expression".into()],
            }),
        }
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task.clear();
        Ok(())
    }
}

/// GreetingMonad:用 inputs.name(默认 "world")生成问候语。
///
/// 不接 LLM。最简单的纯字符串 monad,用于演示"非计算类 monad"也走同一协议。
///
/// ## 输入类型宽容
///
/// `name` 字段在 schema 上声明为 string,但 LLM 在派发时经常把数字当
/// name 传(典型场景:计算 50*60=3000 后,把结果当作 name 打招呼)。
/// `bind` 阶段做 type coercion:
/// - `string`  → 原样
/// - `number`  → `n.to_string()`("3000")
/// - `bool`    → `"true"` / `"false"`
/// - `null` 或缺字段 → 默认 `"world"`
/// - `object` / `array` 或多余字段 → 输入错误
///
/// 这样 validation 不会因为 3000 是 number 而不是 string 就拒收。
#[derive(Default)]
pub struct GreetingMonad {
    task: TaskSlot,
    name: String,
}

impl GreetingMonad {
    /// 直接构造已绑定实例；无 handoff 的 factory 应使用 Default。
    pub fn from_handoff(handoff: &MonadHandoff) -> Result<Self, MonadError> {
        let mut instance = Self::default();
        instance.bind(handoff)?;
        Ok(instance)
    }

    fn bind(&mut self, handoff: &MonadHandoff) -> Result<(), MonadError> {
        // 先在局部变量准备好业务输入;只有 try_bind 成功后才写回 self。
        // 与目录 schema 一致：仅接受标量或缺省值。
        if handoff.inputs.keys().any(|key| key != "name") {
            return Err(MonadError::new(
                "greeting name must be string/number/bool/null; extra fields are not allowed",
            ));
        }
        let name = handoff
            .inputs
            .get("name")
            .map(coerce_to_greeting_name)
            .transpose()?
            .unwrap_or_else(|| "world".to_string());
        let expected = self.monad_id().clone();
        self.task.try_bind(handoff, &expected)?;
        self.name = name;
        Ok(())
    }
}

/// 将目录中允许的标量转为问候对象；对象和数组返回输入错误。
fn coerce_to_greeting_name(v: &serde_json::Value) -> Result<String, MonadError> {
    use serde_json::Value;
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok("world".to_string()),
        Value::Object(_) | Value::Array(_) => Err(MonadError::new(
            "greeting name must be string/number/bool/null",
        )),
    }
}

#[async_trait]
impl MonadLoop for GreetingMonad {
    fn monad_id(&self) -> &MonadId {
        static ID: OnceLock<MonadId> = OnceLock::new();
        ID.get_or_init(|| MonadId::new("greeting"))
    }

    fn catalog_entry(&self) -> &CatalogEntry {
        static ENTRY: OnceLock<CatalogEntry> = OnceLock::new();
        ENTRY.get_or_init(|| CatalogEntry {
            summary: "用 inputs.name(默认 world;接受 string/number/bool)生成问候语,不接 LLM"
                .to_string(),
            visible_tools: vec![],
            keywords: vec![
                "greeting".into(),
                "问候".into(),
                "hello".into(),
                "打招呼".into(),
            ],
        })
    }

    fn default_tool_set(&self) -> Vec<ToolKind> {
        vec![]
    }

    async fn start(&mut self, handoff: MonadHandoff) -> Result<(), MonadError> {
        self.bind(&handoff)
    }

    async fn step(&mut self) -> Result<MonadResult, MonadError> {
        let task = self.task.take()?;
        let greeting = format!("Hello, {}!", self.name);
        let usage = TaskSlot::usage_after(&task, 1, 0);
        let (task_id, parent_task_id, result_slot) = task.into_parts();
        Ok(MonadResult::Completed {
            task_id,
            parent_task_id,
            result_slot,
            monad_id: MonadId::new("greeting"),
            output: serde_json::json!({ "greeting": greeting, "name": self.name.clone() }),
            evidence_refs: vec![],
            usage,
            finished_at_ms: now_ms(),
        })
    }

    async fn cancel(&mut self) -> Result<(), MonadError> {
        self.task.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::protocol::{MonadHandoff, MonadId, ResultSlot, SCHEMA_VERSION_V1};
    use crate::monad::TaskId;

    fn handoff_with(inputs: serde_json::Value) -> MonadHandoff {
        // 接受 Value,转成 MonadHandoff.inputs 需要的 Map<String, Value>
        let inputs_map = match inputs {
            serde_json::Value::Object(m) => m,
            other => panic!("test helper expects JSON object, got {other:?}"),
        };
        MonadHandoff {
            schema_version: SCHEMA_VERSION_V1.into(),
            task_id: TaskId::new(),
            parent_task_id: None,
            monad_id: MonadId::new("greeting"),
            attempt_id: 1,
            objective: "打招呼".into(),
            inputs: inputs_map,
            relevant_context: vec![],
            constraints: vec![],
            completion_criteria: "返回 greeting 字段".into(),
            result_slot: ResultSlot("chat.reply".into()),
            depends_on: vec![],
            budget: Default::default(),
            issued_at_ms: 0,
            trace: crate::monad::protocol::TraceMeta {
                frame_ref: "test".into(),
                request_id: "test".into(),
            },
        }
    }

    /// P1:validation 错误不应再阻断典型多步任务。
    /// 场景:计算 50*60=3000 后用结果打招呼,LLM 把 number 3000 当 name 传。
    /// 之前:`inputs must contain exactly one string field: name` → 任务停摆。
    /// 现在:bind coerce 3000 → "3000" → 继续。
    #[test]
    fn greeting_accepts_number_name_for_step_chaining() {
        let h = handoff_with(serde_json::json!({"name": 3000}));
        let mut m = GreetingMonad::default();
        m.bind(&h).expect("number should coerce to string");
        assert_eq!(m.name, "3000");
    }

    #[test]
    fn greeting_accepts_string_name() {
        let h = handoff_with(serde_json::json!({"name": "weykon"}));
        let mut m = GreetingMonad::default();
        m.bind(&h).unwrap();
        assert_eq!(m.name, "weykon");
    }

    #[test]
    fn greeting_accepts_bool_name() {
        let h = handoff_with(serde_json::json!({"name": true}));
        let mut m = GreetingMonad::default();
        m.bind(&h).unwrap();
        assert_eq!(m.name, "true");
    }

    #[test]
    fn greeting_defaults_when_name_missing() {
        let h = handoff_with(serde_json::json!({}));
        let mut m = GreetingMonad::default();
        m.bind(&h).unwrap();
        assert_eq!(m.name, "world");
    }

    #[test]
    fn greeting_defaults_when_name_null() {
        let h = handoff_with(serde_json::json!({"name": null}));
        let mut m = GreetingMonad::default();
        m.bind(&h).unwrap();
        assert_eq!(m.name, "world");
    }

    /// 端到端:模拟你贴的日志场景 —— 数字 name → 整轮 step 跑通,
    /// 输出 greeting 是 "Hello, 3000!" 而不是 error。
    #[tokio::test]
    async fn greeting_full_step_with_number_name_succeeds() {
        let h = handoff_with(serde_json::json!({"name": 3000}));
        let mut m = GreetingMonad::default();
        m.start(h).await.unwrap();
        let result = m.step().await.unwrap();
        let output = match result {
            MonadResult::Completed { output, .. } => output,
            other => panic!("expected Completed, got {other:?}"),
        };
        assert_eq!(output["name"], "3000");
        assert_eq!(output["greeting"], "Hello, 3000!");
    }
    #[tokio::test]
    async fn greeting_rejects_invalid_schema_without_binding_task() {
        let mut monad = GreetingMonad::default();
        for inputs in [
            serde_json::json!({"name":{}}),
            serde_json::json!({"name":[]}),
            serde_json::json!({"name":"Ada","extra":1}),
        ] {
            assert!(monad.start(handoff_with(inputs)).await.is_err());
            assert!(monad.step().await.is_err());
        }
        monad
            .start(handoff_with(serde_json::json!({"name":3000})))
            .await
            .unwrap();
        assert!(matches!(
            monad.step().await.unwrap(),
            MonadResult::Completed { .. }
        ));
    }
}
