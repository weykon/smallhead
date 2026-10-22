//! YOLO policy + dangerous command patterns.
//!
//! v0.4 第一步：硬编码黑名单（substring match）。
//! 后续版本会换成可配置 policy + Safe/Sandbox tier。

/// YOLO policy：当前唯一支持 tier = 黑名单拦截。
///
/// 设计意图：把 policy 抽象成 struct，方便以后加字段（tier、allowlist、
/// audit sink）而不破坏 API。即使现在只支持黑名单，调用方也应该写
/// `YoloPolicy::default().check_command(cmd)` 而不是直接调自由函数。
#[derive(Debug, Clone)]
pub struct YoloPolicy {
    /// 是否启用黑名单（默认 true）。给将来"完全关掉检查"的逃生口留位。
    pub blacklist_enabled: bool,
}

impl Default for YoloPolicy {
    fn default() -> Self {
        // 默认开启黑名单 —— 安全优先。
        // 显式 `blacklist_enabled: false` 才能关掉（逃生口）。
        Self {
            blacklist_enabled: true,
        }
    }
}

impl YoloPolicy {
    /// 检查命令是否安全。
    /// 返回 `Ok(())` 表示通过；`Err(PermissionError::BlacklistedPattern)` 表示拒绝。
    pub fn check_command(&self, command: &str) -> Result<(), crate::check::PermissionErrorInner> {
        if !self.blacklist_enabled {
            return Ok(());
        }
        crate::check::check_command(command)
    }
}

/// 危险命令黑名单。根目录/家目录规则检查路径结束边界，其余为字面子串。
pub const DANGEROUS_PATTERNS: &[&str] = &[
    // 递归删除根目录或家目录；不包含它们的普通子路径。
    "rm -rf /",
    "rm -rf /*",
    "rm -rf ~",
    // Bash fork bomb
    ":(){:|:&};:",
    // dd 任意 if= 源（写盘神器）
    "dd if=",
    // mkfs / mkfs.ext4 等（擦盘格式化）
    "mkfs",
    // sudo 前导（注意带尾空格，避免命中 "sudoers" 等合法词）
    "sudo ",
    // 全开权限到根
    "chmod -R 777 /",
];
