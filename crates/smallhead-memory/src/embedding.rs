//! 本地 ONNX embedding via fastembed-rs.
//!
//! 默认使用 `BAAI/bge-small-zh-v1.5`(512 维,中文场景)。
//! 首次调用 [`LocalEmbedder::new`] 会从 Hugging Face Hub 拉取模型权重到本地
//! 缓存(`~/.cache/fastembed/`),并由 fastembed 的 `ort-download-binaries`
//! feature 在运行时下载 ONNX Runtime 共享库 —— **不需要用户预先安装
//! ONNX Runtime**。
//!
//! ## 同步 vs 异步
//!
//! fastembed-rs 是同步 API(`TextEmbedding::embed` 是阻塞调用,内部分批跑
//! ONNX inference)。如果需要在异步上下文里调用,直接 `spawn_blocking` 即可:
//!
//! ```ignore
//! let embedder = LocalEmbedder::new()?;
//! let v = tokio::task::spawn_blocking(move || embedder.embed("你好")).await??;
//! ```
//!
//! ## CI / 无网环境
//!
//! 首次调用依赖网络下载。如果网络受限或不想让 CI 下模型,请把这两个测试
//! 保持 `#[ignore]`,本地手动验证:
//!
//! ```sh
//! cargo test -p smallhead-memory -- --ignored embed_smoke embed_batch_shape
//! ```

use fastembed::{InitOptions as FastembedInitOptions, TextEmbedding};

use crate::error::MemoryError;

/// `BAAI/bge-small-zh-v1.5` 模型的向量维度 = 512。
///
/// 同时也是默认 [`LocalEmbedder`] 实例的 [`LocalEmbedder::dim`],用来在
/// LanceDB 里建 schema 时对齐向量列长度。
pub const BGE_SMALL_ZH_DIM: usize = 512;

/// 本地 ONNX embedding 模型句柄。
///
/// 内部包装 fastembed-rs 的 [`TextEmbedding`],目前只接 bge-small-zh-v1.5。
/// 构造一次,反复调 [`LocalEmbedder::embed`] / [`LocalEmbedder::embed_batch`]。
///
/// 模型推理是同步的、阻塞的;调用方应在 `spawn_blocking` 里跑。
pub struct LocalEmbedder {
    model: TextEmbedding,
    dim: usize,
}

impl LocalEmbedder {
    /// 默认加载 `bge-small-zh-v1.5`(512 维)。
    ///
    /// 首次调用耗时较长:从 Hugging Face Hub 拉 ~50 MB 的模型权重,并下载
    /// ONNX Runtime 共享库到 `~/.cache/onnxruntime/`。后续调用走本地缓存,
    /// 通常 < 1 秒。
    ///
    /// # Errors
    ///
    /// - 网络下载失败(无网 / hf-hub 不可达)
    /// - ONNX Runtime 初始化失败
    /// - 模型文件损坏 / 格式错误
    /// 都会映射到 [`MemoryError::Embedding`]。
    pub fn new() -> Result<Self, MemoryError> {
        // BGE 系列对 query / passage 有不同的归一化建议。fastembed 会在内部
        // 处理,我们不强制加前缀 —— 用户在 ingest 阶段按自己的 retrieval
        // 策略加 `query: ` / `passage: ` 前缀即可。
        // NOTE(A-team patch 2026-10-05): fastembed 5 把 `InitOptions` 标成了
        // `#[non_exhaustive]`,不能再用 struct literal 构造;改用 builder。
        // 语义跟原来完全一样 —— B 队后续 PR 可以自行简化。
        let mut opts = FastembedInitOptions::default();
        opts.model_name = fastembed::EmbeddingModel::BGESmallZHV15;
        opts.show_download_progress = false;
        let model = TextEmbedding::try_new(opts).map_err(|e| {
            MemoryError::Embedding(format!("failed to initialise bge-small-zh-v1.5: {e}"))
        })?;
        Ok(Self {
            model,
            dim: BGE_SMALL_ZH_DIM,
        })
    }

    /// 向量维度。对 bge-small-zh-v1.5 永远是 [`BGE_SMALL_ZH_DIM`]。
    #[inline]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 对单段文本做 embedding。
    ///
    /// # Errors
    ///
    /// 任何 ONNX inference 失败都会映射到 [`MemoryError::Embedding`]。
    // NOTE(A-team patch 2026-10-05): fastembed 5 的 `TextEmbedding::embed`
    // 用了内部 mut buffer,需要 `&mut self` 而不是 `&self`。下面两处同步
    // 改签名 —— 调用方 (`spawn_blocking`) 已经是 mut 借的,所以接口变化
    // 不影响现有调用点。
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let docs: Vec<&str> = vec![text];
        let mut out = self
            .model
            .embed(docs, None)
            .map_err(|e| MemoryError::Embedding(format!("embed single failed: {e}")))?;
        // fastembed 的 `embed` 永远返回至少一个 vector;空输入会直接报错而不是返回空。
        out.pop()
            .ok_or_else(|| MemoryError::Embedding("embed returned no vector".into()))
    }

    /// 批量 embedding。比循环调 [`LocalEmbedder::embed`] 快 —— fastembed
    /// 内部按 batch_size 自动分块跑 ONNX inference。
    ///
    /// 返回的 `Vec` 与输入 `texts` 等长且一一对应。
    ///
    /// # Errors
    ///
    /// 任何 ONNX inference 失败都会映射到 [`MemoryError::Embedding`]。
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let out = self
            .model
            .embed(texts.to_vec(), None)
            .map_err(|e| MemoryError::Embedding(format!("embed batch failed: {e}")))?;
        // 长度防御性检查:理论上永远 == texts.len(),但 fastembed 升级时
        // 万一改了语义,这里会暴露出来而不是悄悄错位。
        if out.len() != texts.len() {
            return Err(MemoryError::Embedding(format!(
                "embed batch returned {} vectors for {} inputs",
                out.len(),
                texts.len()
            )));
        }
        Ok(out)
    }
}
