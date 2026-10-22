//! smallhead-permission: command policy + dangerous pattern blacklist.
//!
//! v0.4 第一步：YOLO 模式 + 硬编码黑名单。
//!
//! ## 模块结构
//! - [`policy`] —— `YoloPolicy` struct + `DANGEROUS_PATTERNS` 常量
//! - [`check`] —— `check_command()` substring matcher
//! - [`error`] —— `PermissionError` (thiserror)
//!
//! ## 用法（core 侧）
//! ```ignore
//! use smallhead_permission::check_command;
//!
//! if let Err(e) = check_command(&command) {
//!     return Ok(ToolResult {
//!         is_error: true,
//!         content: vec![ContentBlock::Text {
//!             text: format!("BLOCKED by permission: {e}"),
//!         }],
//!         ..
//!     });
//! }
//! ```

pub mod check;
pub mod error;
pub mod policy;

pub use check::check_command;
pub use error::PermissionError;
pub use policy::{YoloPolicy, DANGEROUS_PATTERNS};
