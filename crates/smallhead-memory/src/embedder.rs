//! `Embedder` trait — the abstraction D-ingest team (D 队) consumes.
//!
//! ## 背景
//!
//! ingest 管道需要把一段文本转成 `Vec<f32>`,但不想被绑死在 fastembed / bge-small-zh-v1.5
//! 上 —— 测试要换 mock embedder,后续 production 可能换 remote embedding API。
//! `Embedder` trait 就是这个抽象。
//!
//! ## 设计要点
//!
//! - **`Send` 而不是 `Sync`**:fastembed 的 `TextEmbedding::embed` 内部需要 `&mut self`,
//!   而且它的 ONNX session 不是线程安全的;大多数 embedding 后端都要求独占访问。
//!   所以 trait 不强制 `Sync`,只强制 `Send`(让 `IngestPipeline` 能跨 `.await` 持有)。
//! - **`dim()` 必须 == [`crate::store::EMBEDDING_DIM`]** —— LanceDB schema 写死 512,
//!   embedder dim 不一致会导致 FixedSizeList 列对不上而 panic。这个一致性由 D-ingest
//!   团队在 `IngestPipeline::new` 阶段 assert,见 [`IngestPipeline::new`]。
//! - **`embed_batch` 有默认实现**:循环 `embed`,对小批量足够;对性能敏感的后端可以
//!   override 走 batch API(`LocalEmbedder` 已经是 override 过的)。
//! - **`&mut self`**:和 fastembed 5 的 `TextEmbedding::embed` 内部 mut buffer 对齐;
//!   也让 trait impl 更宽松(比如 mock 不需要内部状态也能满足 `&mut self`)。
//!
//! ## 跟 B 队的关系
//!
//! B 队写的 [`crate::LocalEmbedder`] 在这个文件里挂 `impl Embedder for LocalEmbedder`,
//! 没有任何 `&self` → `&mut self` 的额外签名变化 —— 因为 `LocalEmbedder::embed` 本来
//! 就是 `&mut self`,正好对得上。

use crate::error::MemoryError;

/// 把一段文本转成定长 embedding 向量的抽象。
///
/// 所有实现必须满足:
/// - `dim()` 跟 LanceDB schema 一致(默认 512)。
/// - 同一段文本在同一个实例上调多次 `embed`,结果**逐 bit 一致**(deterministic)——
///   否则 vector_search 的 "ingest 回去 search 命中自己" 会变成 flaky test。
///
/// 没有要求 `Sync`(见 module docs)。
pub trait Embedder: Send {
    /// 输出向量的维度。必须等于 [`crate::store::EMBEDDING_DIM`] 才会被 `IngestPipeline`
    /// 接受(`IngestPipeline::new` 会 assert)。
    fn dim(&self) -> usize;

    /// 对单段文本做 embedding。
    ///
    /// 返回的向量长度 == `dim()`,且一般应做 L2 normalize(对 cosine similarity 检索
    /// 友好)。Mock 实现和 `LocalEmbedder` 都做 normalize。
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, MemoryError>;

    /// 批量 embedding。返回的 `Vec` 与 `texts` 等长且一一对应。
    ///
    /// 默认实现:循环 `embed`。性能敏感的实现可以 override 走底层 batch API
    /// (fastembed 的 `TextEmbedding::embed` 一次喂一批比循环快很多)。
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t)?);
        }
        Ok(out)
    }
}

impl Embedder for crate::embedding::LocalEmbedder {
    fn dim(&self) -> usize {
        crate::embedding::LocalEmbedder::dim(self)
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>, MemoryError> {
        crate::embedding::LocalEmbedder::embed(self, text)
    }

    /// 直接走 fastembed 的 batch API,不走默认实现的循环 —— 性能好得多。
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        crate::embedding::LocalEmbedder::embed_batch(self, texts)
    }
}
