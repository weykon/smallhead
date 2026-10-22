//! 管理器级资源预算；调用方不能用单条命令绕过这些上限。
use crate::ProcessError;
use std::time::Duration;

/// 同一个管理器共享的执行限制。
#[derive(Debug, Clone)]
pub struct ProcessLimits {
    /// 活跃执行（运行及排队）的上限。
    pub max_active: usize,
    /// 保留的执行记录上限；达到后须显式清理已完成记录。
    pub max_records: usize,
    /// 整个日志根目录的预算；启动时为输出及审计预留空间。
    pub max_disk_bytes: u64,
    /// 同时持有 OS 子进程的最大数量；排队不占名额。
    pub max_concurrent: usize,
    /// 每条命令 stdout、stderr 各自最多保存的字节数。
    /// 超出后停止进程组，保留已写入的前缀并记录 OutputLimit。
    pub max_stream_bytes: u64,
    /// 未指定 timeout 时采用的执行时限，不包含排队时间。
    pub default_timeout: Duration,
    /// 单条命令允许指定的最大执行时限。
    pub max_timeout: Duration,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            max_active: 32,
            max_records: 1024,
            max_disk_bytes: 512 * 1024 * 1024,
            max_concurrent: 4,
            max_stream_bytes: 4 * 1024 * 1024,
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
        }
    }
}

impl ProcessLimits {
    pub(crate) fn validate(&self) -> Result<(), ProcessError> {
        if self.max_concurrent == 0
            || self.max_active < self.max_concurrent
            || self.max_records < self.max_active
            || self
                .max_stream_bytes
                .checked_mul(2)
                .and_then(|n| n.checked_add(131072))
                .is_none_or(|n| n > self.max_disk_bytes)
            || self.max_concurrent > tokio::sync::Semaphore::MAX_PERMITS
            || self.max_stream_bytes == 0
            || self.default_timeout.is_zero()
            || self.default_timeout > self.max_timeout
        {
            return Err(ProcessError::InvalidSpec("无效的进程资源限制".into()));
        }
        Ok(())
    }
}
