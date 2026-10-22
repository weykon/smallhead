//! Input specification for a managed execution.
//!
//! [`ProcessSpec`] is what callers hand to
//! [`crate::manager::ProcessManager::start`]. It is intentionally
//! declarative — the executor decides whether to use explicit `sh -c` mode or
//! direct `program + args` mode without changing the lifecycle API.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ProcessError;

/// Declarative description of one execution attempt.
///
/// The executor composes a minimal environment, applies cwd and timeout, then
/// retains ownership until the child and its output readers are reaped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    /// 直接启动程序；设置后 command 必须为空，args 不经 shell 展开。
    #[serde(default)]
    pub program: Option<String>,
    /// 直接启动的参数，保留参数边界。
    #[serde(default)]
    pub args: Vec<String>,
    /// Command string passed to `sh -c`. Mirrors the current `BashTool`
    /// behavior so the S1b migration of BashTool onto `ProcessManager`
    /// is a pure refactor for callers.
    pub command: String,

    /// Working directory. Relative paths are resolved against the host
    /// process cwd; `None` inherits that cwd. Agent adapters must supply
    /// their explicit workspace cwd when integrating this manager.
    pub cwd: Option<PathBuf>,

    /// Explicit environment additions. The executor composes these on top of a
    /// cleared base (currently just `PATH`) rather than inheriting the
    /// full parent environment.
    #[serde(skip_serializing, default)]
    pub env: BTreeMap<String, String>,

    /// Hard deadline. `None` adopts the manager's default timeout. A value of
    /// `Some(Duration::ZERO)` is rejected by [`ProcessSpec::validate`] —
    /// zero-duration timeouts would race the spawn itself.
    pub timeout: Option<Duration>,

    /// Lifecycle scope — see [`ExecutionScope`]. Public command execution
    /// accepts `TaskBound`; ServiceManager internally authorizes
    /// `SessionService`. Persistent services remain unsupported.
    pub scope: ExecutionScope,

    /// Human-readable description (purpose, intent). Optional but
    /// encouraged so log lines and TUI rows can render something better
    /// than "command: sh -c ...".
    pub description: Option<String>,
}

impl ProcessSpec {
    /// Construct a minimal spec — useful for tests and one-shot callers
    /// that don't care about cwd/env/timeout/scope.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            program: None,
            args: vec![],
            command: command.into(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: None,
            scope: ExecutionScope::TaskBound,
            description: None,
        }
    }

    /// 直接执行程序及参数，不经过 sh -c。
    pub fn program(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: Some(program.into()),
            args,
            ..Self::new("")
        }
    }

    /// 人类可读命令，仅用于显示和现有权限规则，不用于再次 shell 执行。
    pub fn command_display(&self) -> String {
        match &self.program {
            Some(program) => std::iter::once(program.clone())
                .chain(self.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            None => self.command.clone(),
        }
    }

    /// Builder-style setter for [`Self::timeout`]. Convenience for callers
    /// who want to chain off [`ProcessSpec::new`]:
    ///
    /// ```ignore
    /// ProcessSpec::new("sleep 5").with_timeout(Duration::from_millis(200))
    /// ```
    ///
    /// Validation still happens in [`ProcessSpec::validate`] — zero-duration
    /// timeouts are rejected there.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Validate a public finite-command spec.
    ///
    /// Rejects:
    /// - empty `command`
    /// - `timeout == Some(Duration::ZERO)` (would race the spawn)
    /// - any scope other than TaskBound (session services enter through the
    ///   dedicated ServiceManager ownership path)
    pub fn validate(&self) -> Result<(), ProcessError> {
        if self
            .program
            .as_ref()
            .is_some_and(|p| p.trim().is_empty() || !self.command.is_empty())
            || (self.program.is_none() && (self.command.trim().is_empty() || !self.args.is_empty()))
        {
            return Err(ProcessError::InvalidSpec(
                "command must not be empty".to_string(),
            ));
        }

        if let Some(t) = self.timeout {
            if t.is_zero() {
                return Err(ProcessError::InvalidSpec(
                    "timeout must be greater than zero".to_string(),
                ));
            }
        }

        if !matches!(self.scope, ExecutionScope::TaskBound) {
            return Err(ProcessError::InvalidSpec(
                "public execution only supports scope=TaskBound; use ServiceManager for session services"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

/// Lifecycle scope of an execution.
///
/// Public finite commands accept [`ExecutionScope::TaskBound`]. The service
/// manager is the only entry point authorized to use `SessionService`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionScope {
    /// Lifetime tied to the parent task — cancelled/cleaned up when the
    /// task ends. This is the public finite-command scope.
    TaskBound,

    /// Lives across chat turns, cleaned up on normal application exit.
    /// Lives across chat turns and is owned by ServiceManager.
    SessionService,

    /// Lives across application restarts. Requires a supervisor or
    /// verifiable recovery mechanism. It is rejected so callers cannot
    /// request a guarantee the manager cannot make.
    PersistentService,
}
