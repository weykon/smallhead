//! Agent 和 TUI 共用的有界输出读取契约。
use serde::{Deserialize, Serialize};

/// 需要读取的输出流，不能提供任意文件路径。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// 标准输出。
    Stdout,
    /// 标准错误。
    Stderr,
    /// 持久化生命周期事件 JSONL。
    Events,
}

impl OutputStream {
    pub(crate) fn filename(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log",
            Self::Stderr => "stderr.log",
            Self::Events => "events.jsonl",
        }
    }
}

/// 一页输出。偏移量按字节计算，原始 bytes 保留无效 UTF-8。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPage {
    /// 原始输出字节，最多 64 KiB。
    pub bytes: Vec<u8>,
    /// 便于显示的文本；无效 UTF-8 会替换为替代字符。
    pub text: String,
    /// 下一页的字节偏移。
    pub next_offset: u64,
    /// 本次快照的已保存文件长度。
    pub stored_bytes: u64,
    /// 是否已读到本次文件快照结尾；运行中的流仍可能继续增长。
    pub eof: bool,
    /// 进程是否已经终态；不把临时 EOF 当作任务完成。
    pub complete: bool,
    /// 此执行是否因输出超过预算而终止。
    pub output_limited: bool,
}

/// 每次读取最多返回的原始字节数。
pub const MAX_OUTPUT_PAGE_BYTES: usize = 64 * 1024;
