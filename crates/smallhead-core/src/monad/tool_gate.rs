//! 工具权限网关(`ToolGate`)—— 运行时检查 monad 调用的工具是否被允许。
//!
//! 对应 `docs/monad-orchestration-pattern.md` §2.3。
//!
//! ## 职责
//!
//! - 由 Scheduler 在 spawn MonadLoop 时根据 handoff 构造。
//! - **不引入 async / tokio / runtime** —— 只在内存里做 `HashMap` 查询。
//! - 公开 API 全用 `&self` / `&mut self`,内部用 `HashMap`。
//! - `check(&ToolCall)` 不克隆 `ToolCall`,只读借用。
//!
//! ## 不变量
//!
//! - 具体类型,非 trait。
//! - `default_deny = true` → 白名单模式;`default_deny = false` → 黑名单模式。
//! - `allowed` 以 `ToolKind` 作 key(compile-time 防 name 冲突)。
//!
//! ## TODO(Slice 4)
//!
//! `filter()` 当前返回 `Vec<ToolKind>`,而 pattern doc §2.3 写的是
//! `Vec<ToolSchema>`(= `ToolSpec`)。MVP 第一片先用 `ToolKind` 清单,保持与
//! `tool.rs` 的 schema 概念解耦。Slice 4 决定:要把 `ToolSpec` 也带回 filter() 的
//! 返回值(给 LLM 看的工具表),还是让调用方自己拿 kind 再调 `ToolKind::spec()`。

use std::collections::HashMap;
use std::fmt;

use crate::tool::ToolKind;

/// 工具权限位标记。
///
/// MVP 用 3 个 bool + 一个 quota:比 fine-grained ACL 简单,够用。
/// 后续要加 per-arg 校验(比如 Read 只能读某个目录)再扩。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPermissions {
    /// 是否允许读取类操作(Read / Bash 的 read 命令等)。
    pub can_read: bool,
    /// 是否允许写入类操作(Write / Edit / Bash 的写命令等)。
    pub can_write: bool,
    /// 是否允许网络访问(WebFetch / Bash 的网络命令等)。
    pub can_network: bool,
    /// 单步允许的最大调用次数。0 = 完全禁止。
    pub max_calls_per_step: u32,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        // 全允许,quota 默认 10(给 "能用,但别滥用" 的合理默认)
        Self {
            can_read: true,
            can_write: true,
            can_network: true,
            max_calls_per_step: 10,
        }
    }
}

/// Gate 拒绝原因。
///
/// 现在 `ToolKind` derive 了 `Hash + Eq + PartialEq + Debug`,所以可以直接用
/// `#[derive(Debug)]`,不再需要手写 Debug impl。
#[derive(Debug, Clone)]
pub enum GateError {
    /// 工具被完全禁止(在 allowed 集合外 / 被 forbid)。
    Forbidden(ToolKind, &'static str),
    /// 工具存在,但本步调用次数已超 quota。
    OverQuota(ToolKind),
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forbidden(kind, reason) => {
                write!(f, "tool forbidden: {} ({})", kind.name(), reason)
            }
            Self::OverQuota(kind) => write!(f, "tool over quota: {}", kind.name()),
        }
    }
}

impl std::error::Error for GateError {}

/// 最小调用描述 —— 进 `check()` 用,不克隆。
///
/// `name` 是 caller 提供的字符串(对应 `ToolKind::name()` 的结果),
/// 保留下来是为了日志/调试。Gate 自身用 `kind` 作 HashMap 查询(类型安全的 key)。
///
/// 设计选择:之前 `ToolKind` 没 derive `Hash + Eq`,所以走 `name` 字符串路径;
/// 现在 ToolKind 已 derive 这两个 trait,Gate 内部 key 改为 `ToolKind`,
/// 拿到 compile-time 防 name 冲突的能力。
#[derive(Debug, Clone)]
pub struct ToolCall<'a> {
    /// 工具种类(用于错误信息、日志、HashMap 查询)。
    pub kind: ToolKind,
    /// 工具名(`ToolKind::name()` 的字符串)。
    pub name: &'a str,
}

/// 工具权限网关。
///
/// - `default_deny = true` → 默认禁止所有工具(白名单模式)
/// - `default_deny = false` → 默认允许所有工具(黑名单模式)
///
/// `allowed` 以 `ToolKind` 作 key(compile-time 防 name 冲突)。
/// `check()` 决定某个 ToolKind 是否在白名单/黑名单内:
/// - `default_deny=true` + kind 不在 allowed → Forbidden
/// - `default_deny=false` + kind 不在 allowed → 视为全允许
/// - 在 allowed → 按 ToolPermissions 逐项检查
#[derive(Debug, Clone)]
pub struct ToolGate {
    /// 已显式配置的 tool 权限集合(key = ToolKind,不再是 String)。
    allowed: HashMap<ToolKind, ToolPermissions>,
    /// 默认拒绝?true=白名单模式,false=黑名单模式。
    default_deny: bool,
}

impl ToolGate {
    /// 新建 gate。`default_deny` 决定白名单/黑名单默认行为。
    pub fn new(default_deny: bool) -> Self {
        Self {
            allowed: HashMap::new(),
            default_deny,
        }
    }

    /// 设置某个 tool 的权限。覆盖之前的配置。
    pub fn allow(&mut self, kind: ToolKind, perms: ToolPermissions) {
        self.allowed.insert(kind, perms);
    }

    /// 一键禁止某个 tool。
    ///
    /// 等价于 `allow(kind, ToolPermissions { can_read: false, can_write: false,
    /// can_network: false, max_calls_per_step: 0 })`。
    /// 语义上更明确(MVP 推荐用法)。
    pub fn forbid(&mut self, kind: ToolKind) {
        self.allowed.insert(
            kind,
            ToolPermissions {
                can_read: false,
                can_write: false,
                can_network: false,
                max_calls_per_step: 0,
            },
        );
    }

    /// 根据当前 allowed 集合 + default_deny,过滤出"可见"工具。
    ///
    /// TODO(Slice 4): 当前返回 `Vec<ToolKind>`。
    /// pattern doc §2.3 写的是 `Vec<ToolSchema>`(= `ToolSpec`)。
    /// 决定权在 Slice 4:要不要在 filter 阶段就把 spec 也带回来?
    pub fn filter(&self, available: &[ToolKind]) -> Vec<ToolKind> {
        available
            .iter()
            .filter(|kind| {
                if self.default_deny {
                    // 白名单模式:kind 必须在 allowed 里
                    self.allowed.contains_key(kind)
                } else {
                    // 黑名单模式:不在 forbid 集合里
                    // 启发:不在 allowed 字典里 → 全允许;
                    // 在 allowed 但配额/所有位都 0 → 视为禁止
                    match self.allowed.get(kind) {
                        None => true,
                        Some(p) => {
                            p.max_calls_per_step > 0 || p.can_read || p.can_write || p.can_network
                        }
                    }
                }
            })
            .cloned()
            .collect()
    }

    /// 单次执行前检查。
    ///
    /// - `default_deny=true` + kind 不在 allowed → `Forbidden(kind, "not in allowlist")`
    /// - `default_deny=false` + kind 不在 allowed → 视为允许
    /// - 在 allowed 但 max_calls_per_step=0 → `OverQuota`
    /// - 在 allowed 且 quota > 0 → OK(MVP 不区分 read/write 调用)
    pub fn check(&self, call: &ToolCall<'_>) -> Result<(), GateError> {
        match self.allowed.get(&call.kind) {
            None => {
                if self.default_deny {
                    Err(GateError::Forbidden(call.kind.clone(), "not in allowlist"))
                } else {
                    Ok(())
                }
            }
            Some(perms) => {
                if perms.max_calls_per_step == 0 {
                    Err(GateError::OverQuota(call.kind.clone()))
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// default_deny=true + 没 allow → 任何 check 都 Forbidden。
    #[test]
    fn gate_default_deny_blocks_everything() {
        let gate = ToolGate::new(true);
        let bash = ToolKind::Bash(crate::BashTool);
        let call = ToolCall {
            kind: bash.clone(),
            name: bash.name(),
        };
        match gate.check(&call) {
            Err(GateError::Forbidden(k, _)) => assert_eq!(k.name(), "bash"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    /// allow 只读:max_calls_per_step=0 → OverQuota。
    #[test]
    fn gate_allow_read_but_not_write() {
        let mut gate = ToolGate::new(true);
        let read = ToolKind::Read(crate::ReadTool);
        gate.allow(
            read.clone(),
            ToolPermissions {
                can_read: true,
                can_write: false,
                can_network: false,
                max_calls_per_step: 0,
            },
        );
        let call = ToolCall {
            kind: read.clone(),
            name: read.name(),
        };
        match gate.check(&call) {
            Err(GateError::OverQuota(k)) => assert_eq!(k.name(), "read"),
            other => panic!("expected OverQuota, got {other:?}"),
        }
        // 写工具不在 allowed → Forbidden
        let write = ToolKind::Write(crate::WriteTool);
        let w_call = ToolCall {
            kind: write.clone(),
            name: write.name(),
        };
        assert!(matches!(
            gate.check(&w_call),
            Err(GateError::Forbidden(_, _))
        ));
    }

    /// filter 从 6 个 ToolKind 中挑出 allowed 的 3 个。
    #[test]
    fn gate_filter_returns_subset() {
        let mut gate = ToolGate::new(true);
        let allowed_kinds = [
            ToolKind::Echo(crate::EchoTool),
            ToolKind::Add(crate::AddTool),
            ToolKind::Sub(crate::SubTool),
        ];
        for k in &allowed_kinds {
            gate.allow(k.clone(), ToolPermissions::default());
        }
        let available = vec![
            ToolKind::Echo(crate::EchoTool),
            ToolKind::Add(crate::AddTool),
            ToolKind::Sub(crate::SubTool),
            ToolKind::Mul(crate::MulTool),
            ToolKind::Div(crate::DivTool),
            ToolKind::Calculate(crate::CalculateTool),
        ];
        let filtered = gate.filter(&available);
        assert_eq!(filtered.len(), 3);
        let filtered_names: Vec<&str> = filtered.iter().map(|k| k.name()).collect();
        assert!(filtered_names.contains(&"echo"));
        assert!(filtered_names.contains(&"add"));
        assert!(filtered_names.contains(&"sub"));
    }

    /// max_calls_per_step=0 + can_read=true → 仍然 OverQuota(MVP 约定:0 quota = 全禁)。
    #[test]
    fn gate_max_calls_per_step_zero_blocks() {
        let mut gate = ToolGate::new(true);
        let read = ToolKind::Read(crate::ReadTool);
        gate.allow(
            read.clone(),
            ToolPermissions {
                can_read: true,
                can_write: true,
                can_network: true,
                max_calls_per_step: 0,
            },
        );
        let call = ToolCall {
            kind: read.clone(),
            name: read.name(),
        };
        match gate.check(&call) {
            Err(GateError::OverQuota(_)) => {}
            other => panic!("expected OverQuota, got {other:?}"),
        }
    }

    /// GateError::Forbidden 的 Display 输出非空且含工具名。
    #[test]
    fn gate_error_display() {
        let bash = ToolKind::Bash(crate::BashTool);
        let err = GateError::Forbidden(bash.clone(), "forbidden");
        let s = format!("{err}");
        assert!(!s.is_empty());
        assert!(s.contains("bash"), "Display 应含工具名 'bash': {s}");
        assert!(s.contains("forbidden"), "Display 应含 reason: {s}");

        // OverQuota 也测一下
        let read = ToolKind::Read(crate::ReadTool);
        let err2 = GateError::OverQuota(read);
        let s2 = format!("{err2}");
        assert!(s2.contains("read"));
    }
}
