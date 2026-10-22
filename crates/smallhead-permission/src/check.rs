//! Literal command guardrails with path boundaries for root/home rules.
//! This is not a shell parser or a security sandbox.

use crate::error::PermissionError;

/// 内层 Permission error（crate-internal re-export so policy.rs doesn't reach into error.rs paths).
pub type PermissionErrorInner = PermissionError;

/// 检查命令是否触发黑名单；根目录和家目录规则不匹配子路径。
pub fn check_command(command: &str) -> Result<(), PermissionError> {
    use crate::policy::DANGEROUS_PATTERNS;
    for &pattern in DANGEROUS_PATTERNS {
        let matches = if matches!(
            pattern,
            "rm -rf /" | "rm -rf /*" | "rm -rf ~" | "chmod -R 777 /"
        ) {
            command.match_indices(pattern).any(|(offset, _)| {
                command[offset + pattern.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| {
                        c.is_whitespace()
                            || matches!(c, ';' | '&' | '|' | '<' | '>' | ')' | '\'' | '"' | '`')
                    })
            })
        } else {
            command.contains(pattern)
        };
        if matches {
            return Err(PermissionError::BlacklistedPattern {
                pattern,
                command: command.to_string(),
            });
        }
    }
    Ok(())
}

// ============================ Tests ============================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::YoloPolicy;

    // ----- 正例（应被命中）-----

    #[test]
    fn matches_rm_rf_root() {
        let err = check_command("rm -rf /").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "rm -rf /");
            }
        }
    }

    #[test]
    fn allows_root_and_home_subpaths() {
        for command in [
            "rm -rf /tmp/test",
            "rm -rf /tmp/foo",
            "rm -rf ~/project/cache",
            "rm -rf /tmp/测试",
            "chmod -R 777 /tmp/test",
        ] {
            assert!(check_command(command).is_ok(), "{command}");
        }
    }

    #[test]
    fn blocks_paths_before_shell_delimiters_and_later_matches() {
        for command in [
            "rm -rf /; echo done",
            "rm -rf / && echo done",
            "rm -rf /\n",
            "rm -rf /|cat",
            "rm -rf / >/tmp/out",
            "(rm -rf /)",
            "sh -c 'rm -rf /'",
            "rm -rf ~; echo done",
            "rm -rf /* --no-preserve-root",
            "chmod -R 777 /; echo done",
            "rm -rf /tmp/test; rm -rf /",
        ] {
            assert!(check_command(command).is_err(), "{command}");
        }
    }

    #[test]
    fn matches_rm_rf_root_star() {
        let err = check_command("rm -rf /*").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "rm -rf /*");
            }
        }
    }

    #[test]
    fn matches_rm_rf_home() {
        let err = check_command("rm -rf ~").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "rm -rf ~");
            }
        }
    }

    #[test]
    fn matches_fork_bomb() {
        let err = check_command(":(){:|:&};:").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, ":(){:|:&};:");
            }
        }
    }

    #[test]
    fn matches_dd_if() {
        let err = check_command("dd if=/dev/zero of=/tmp/x").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "dd if=");
            }
        }
    }

    #[test]
    fn matches_mkfs() {
        let err = check_command("mkfs.ext4 /dev/sda1").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "mkfs");
            }
        }
    }

    #[test]
    fn matches_sudo_prefix() {
        // 注意：模式是 "sudo " 带尾空格，不命中 "sudoers"
        let err = check_command("sudo apt install curl").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "sudo ");
            }
        }
    }

    #[test]
    fn matches_chmod_recursive_world() {
        let err = check_command("chmod -R 777 /").unwrap_err();
        match err {
            PermissionError::BlacklistedPattern { pattern, .. } => {
                assert_eq!(pattern, "chmod -R 777 /");
            }
        }
    }

    // ----- 反例（应通过）-----

    #[test]
    fn allows_ls_la_root() {
        // ls -la / 没有 "rm -rf /" 这段子串（grep 而非 prefix）
        assert!(check_command("ls -la /").is_ok());
    }

    #[test]
    fn allows_echo() {
        assert!(check_command("echo hello world").is_ok());
    }

    #[test]
    fn allows_cat() {
        assert!(check_command("cat /etc/hosts").is_ok());
    }

    #[test]
    fn allows_grep_recursive() {
        // grep -r 不会和任何 dangerous pattern 重叠
        assert!(check_command("grep -r 'TODO' src/").is_ok());
    }

    #[test]
    fn allows_sudoers_word() {
        // "sudo " 模式带尾空格，"sudoers" 不是 "sudo " 子串
        assert!(check_command("cat /etc/sudoers").is_ok());
    }

    #[test]
    fn allows_chmod_normal() {
        assert!(check_command("chmod 755 ./bin").is_ok());
    }

    #[test]
    fn allows_normal_rm() {
        // rm 单文件（没 -rf /）应放行
        assert!(check_command("rm /tmp/foo").is_ok());
    }

    #[test]
    fn allows_downloads_and_package_installation() {
        for command in [
            "curl -fsSL https://example.com/install.md",
            "wget -O install.md https://example.com/install.md",
            "echo wget curl",
            "python3 -m pip install --user agent-reach",
            "uv tool install agent-reach",
            "npm install --global example-cli",
            "brew install curl",
        ] {
            assert!(check_command(command).is_ok(), "{command}");
        }
    }

    // ----- YoloPolicy struct 测试 -----

    #[test]
    fn policy_default_blocks_blacklist() {
        let p = YoloPolicy::default();
        assert!(p.check_command("rm -rf /").is_err());
        assert!(p.check_command("echo hello").is_ok());
    }

    #[test]
    fn policy_with_blacklist_disabled_allows_everything() {
        // 逃生口：blacklist_enabled=false 时完全跳过检查
        let p = YoloPolicy {
            blacklist_enabled: false,
        };
        assert!(p.check_command("rm -rf /").is_ok());
        assert!(p.check_command(":(){:|:&};:").is_ok());
    }
}
