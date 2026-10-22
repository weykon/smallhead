//! 中文友好的滑动窗口 chunker。
//!
//! 按 Unicode scalar values (Rust `char`) 切,而不是按 byte 切,所以中文、emoji 等
//! 多字节字符不会被切断。优先在 sentence 边界字符(`.`/`!`/`?`/`!`/`?`/换行
//! 等)切;若边界超出窗口上限则退回 char 边界硬切。
//!
//! # 设计
//! - 默认 `chunk_size = 512 chars`,`overlap = 64 chars`(与 `IngestConfig` 默认对齐)。
//! - 滑动窗口:`start_{i+1} = end_i - overlap`,直至走完。
//! - boundary 优先级:对每个候选窗口 `[start, end)`,在窗口范围内找最后一个 boundary char,
//!   若它距 `end` 不超过 `chunk_size / 4 chars`,则把 `end` 移到该 boundary 字符的下一个 char index。
//!   否则保持硬切。
//! - 不做 unicode sentence segmentation(避免引入 ICU / unicode-segmentation 重依赖)。
//!
//! # 不变量
//! - `overlap < chunk_size`(在 `new()` 中 assert,违反直接 panic)。
//! - 任意 `Chunk.text.trim().is_empty()` 的 chunk 被丢弃。
//! - 空文本返回空 `Vec`。

/// 默认 chunk 大小(char 数)。
pub const DEFAULT_CHUNK_SIZE: usize = 512;

/// 默认 chunk 重叠(char 数)。
pub const DEFAULT_OVERLAP: usize = 64;

/// 一个 chunk 的输出:原文 + 在原文中 `[start, end)` 的 char 区间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// chunk 文本。
    pub text: String,
    /// 起始 char index(inclusive)。
    pub start: usize,
    /// 结束 char index(exclusive)。
    pub end: usize,
}

/// 滑动窗口 chunker。
#[derive(Debug, Clone)]
pub struct Chunker {
    chunk_size: usize,
    overlap: usize,
}

impl Chunker {
    /// 新建一个 chunker。`overlap < chunk_size` 必须满足,否则 panic(明确消息)。
    pub fn new(chunk_size: usize, overlap: usize) -> Self {
        assert!(
            overlap < chunk_size,
            "Chunker::new: overlap ({overlap}) must be < chunk_size ({chunk_size})"
        );
        Self {
            chunk_size,
            overlap,
        }
    }

    /// 默认配置(`DEFAULT_CHUNK_SIZE` / `DEFAULT_OVERLAP`)。
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE, DEFAULT_OVERLAP)
    }

    /// chunk 大小(char 数)。
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// overlap 大小(char 数)。
    pub fn overlap(&self) -> usize {
        self.overlap
    }

    /// 切分文本,使用默认 boundary 集合:`.` `!` `?` `。` `！` `?` `？` `\n`。
    pub fn split(&self, text: &str) -> Vec<Chunk> {
        const DEFAULT_BOUNDARIES: [char; 8] = ['.', '!', '?', '。', '！', '?', '？', '\n'];
        self.split_with_boundaries(text, &DEFAULT_BOUNDARIES)
    }

    /// 切分文本,并在指定 boundary 字符上优先切(默认 `.!? ?!！？ \n`)。
    pub fn split_with_boundaries(&self, text: &str, boundaries: &[char]) -> Vec<Chunk> {
        let total = text.chars().count();
        if total == 0 {
            return Vec::new();
        }

        let max_lookback = self.chunk_size / 4;
        let min_chunk = self.chunk_size / 2;
        let mut out = Vec::new();
        let mut start = 0usize;

        loop {
            if start >= total {
                break;
            }

            // 1) 先按硬切确定候选 end。
            let hard_end = (start + self.chunk_size).min(total);

            // 2) 在 [start, hard_end) 内找最后一个 boundary char 的 char index。
            //    切到 boundary+1 当且仅当:
            //      a) resulting_size >= min_chunk(>= chunk_size/2,避免产出过小 chunk)
            //      b) resulting_size > overlap(保证下一窗口能严格前进,否则
            //         会产出完全在当前 chunk 内的"孤儿"overlap chunk)
            //      c) boundary 距 hard_end <= max_lookback chars(spec 要求的
            //         "boundary 不要离 end 太远"约束)
            //    否则硬切 hard_end。
            let end = match last_boundary_char_index(text, start, hard_end, boundaries) {
                Some(boundary_idx) => {
                    let cut_at = boundary_idx + 1;
                    if cut_at <= start {
                        hard_end
                    } else {
                        let dist_from_end = hard_end.saturating_sub(cut_at);
                        let resulting_size = cut_at - start;
                        if resulting_size >= min_chunk
                            && resulting_size > self.overlap
                            && dist_from_end <= max_lookback
                        {
                            cut_at
                        } else {
                            hard_end
                        }
                    }
                }
                None => hard_end,
            };

            // 3) 安全:保证窗口至少前进一步(避免极端配置下死循环)。
            let real_end = end.max(start + 1).min(total);

            let chunk_text: String = text.chars().skip(start).take(real_end - start).collect();
            if !chunk_text.trim().is_empty() {
                out.push(Chunk {
                    text: chunk_text,
                    start,
                    end: real_end,
                });
            }

            if real_end == total {
                break;
            }

            // 4) 下一窗口: start = end - overlap,严格前进。
            let next_start = real_end.saturating_sub(self.overlap);
            start = if next_start <= start {
                real_end
            } else {
                next_start
            };
        }

        out
    }
}

/// 在 `[start, end)` char 区间内找最后一个 boundary char 的 char index。
///
/// 返回该 boundary 在 `text` 中的 **char index**(不是 byte offset),范围 `[start, end-1]`。
///
/// 若 `boundaries` 为空或 `start >= end` 则返回 `None`。
fn last_boundary_char_index(
    text: &str,
    start: usize,
    end: usize,
    boundaries: &[char],
) -> Option<usize> {
    if boundaries.is_empty() || start >= end {
        return None;
    }
    let mut ci = start;
    let mut last: Option<usize> = None;
    for c in text.chars().skip(start).take(end - start) {
        if boundaries.contains(&c) {
            last = Some(ci);
        }
        ci += 1;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_boundary_returns_correct_index() {
        let text = "abc.def";
        // '.' 在 char index 3
        assert_eq!(last_boundary_char_index(text, 0, 4, &['.']), Some(3));
        // 在 [0, 3) 内没有 '.'
        assert_eq!(last_boundary_char_index(text, 0, 3, &['.']), None);
        // 多个 boundary,返回最后一个
        let text2 = "a.b.c!d";
        assert_eq!(last_boundary_char_index(text2, 0, 7, &['.', '!']), Some(5));
    }

    #[test]
    fn last_boundary_handles_chinese() {
        // "第一句。第二句。" → '。' 在 char index 3
        let text = "第一句。第二句。";
        assert_eq!(last_boundary_char_index(text, 0, 5, &['。']), Some(3));
    }

    #[test]
    fn last_boundary_empty_boundaries() {
        assert_eq!(last_boundary_char_index("abc", 0, 3, &[]), None);
    }
}
