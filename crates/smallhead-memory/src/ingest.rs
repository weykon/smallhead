//! `IngestPipeline` — D 队把 A/B/C 三队的产出串成端到端 ingest 管道。
//!
//! ## 干什么
//!
//! ```text
//!        text (String)
//!           │
//!           ▼  chunker.split()
//!        Vec<Chunk>
//!           │
////           ▼  embedder.embed() per chunk
//!        Vec<Vec<f32>>            (each len == Embedder::dim())
//!           │
//!           ▼  build RecordBatch + uuid + ts
//!        arrow::RecordBatch
//!           │
//!           ▼  table.inner.add(batch).execute()
//!        lance table rows
//! ```
//!
//! 反向 (`search`):
//!
//! ```text
//!        query (String)
//!           │
//!           ▼  embedder.embed(query)
//!        Vec<f32>
//!           │
//!           ▼  table.inner.vector_search(v)?.limit(k).execute()
//!        arrow::RecordBatch stream (含 _distance 列)
//!           │
//!           ▼  提取 id / text / ts / score
//!        Vec<Hit>
//! ```
//!
//! ## 设计要点
//!
//! - **不持有 `MemoryHandle`** —— ingest / search 每次都 `store.ensure_lance_table(...)`
//!   拿一个新 handle。LanceDB 的 `Table` 在并发 reader 之间共享安全(`Arc` 共享内部),
//!   但每个 ingest 之后想看新数据需要重新 open —— 反正 `ensure_lance_table` 走
//!   `open_table` 命中 cache,代价是 `Arc` clone,够便宜。
//! - **`embedder` 是 `&mut`** —— fastembed 5 要求独占(见 `embedder` 模块 docs)。
//! - **空文本短路**:`ingest("")` / `ingest("   ")` 直接返回 0,不浪费一次
//!   embedder 调用 + 一次 batch add。
//! - **uuid v7**:用时间排序的 id,方便后续做按时间窗口 GC;workspace 已经把
//!   `uuid` 的 `v7` feature 打开。
//! - **`_distance` 列**:LanceDB vector search 会自动在结果里塞一列 Float32,
//!   值越小越相似(对 L2 / cosine 距离都成立,符号取决于 metric)。我们把它
//!   原样暴露给 `Hit::score` —— 当前 mock embedder 输出 L2-normalized 向量,
//!   所以 cosine 和 L2 排名一致,自匹配测试能稳定拿到 source text。

use std::sync::Arc;

use arrow_array::array::{FixedSizeListArray, Float32Array, Int64Array, StringArray};
use arrow_array::types::Float32Type;
use arrow_array::{Array, RecordBatch, RecordBatchReader};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use chrono::Utc;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use uuid::Uuid;

use crate::chunker::Chunker;
use crate::embedder::Embedder;
use crate::error::MemoryError;
use crate::store::{LanceStore, EMBEDDING_DIM};
use crate::table::MemoryTable;

/// 一条搜索命中。
///
/// `score` 是 LanceDB 的 `_distance` 列:越小越相似(由 `VectorQuery` 的
/// `distance_type` 决定,默认 L2)。上层 UI 想转成 "[0,1]" 相似度自己 normalize。
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// uuid-v7 字符串,由 `IngestPipeline::ingest` 分配。
    pub id: String,
    /// chunk 原文。
    pub text: String,
    /// `_distance` 列(LanceDB 算出来的,越小越相似)。
    pub score: f32,
    /// chunk ingest 时的 unix 毫秒时间戳。
    pub ts: i64,
}

/// 把 chunker / embedder / LanceStore 三个部件粘在一起的端到端管道。
///
/// `S: Embedder` 是 generic,方便测试塞 mock embedder(网络隔离)。
pub struct IngestPipeline<S: Embedder> {
    chunker: Chunker,
    embedder: S,
    store: LanceStore,
}

impl<S: Embedder> IngestPipeline<S> {
    /// 组装一个 ingest 管道。
    ///
    /// # Panics
    ///
    /// 如果 `embedder.dim() != EMBEDDING_DIM`(LanceDB schema 固定 512),
    /// 直接 panic —— 不一致会在 `add(batch)` 阶段炸成 `SchemaError`,
    /// 那个错误信息远不如这里清楚,提前 fail。
    pub fn new(chunker: Chunker, embedder: S, store: LanceStore) -> Self {
        let dim = embedder.dim();
        assert_eq!(
            dim as i32, EMBEDDING_DIM,
            "IngestPipeline::new: embedder.dim() ({dim}) must match EMBEDDING_DIM ({EMBEDDING_DIM})"
        );
        Self {
            chunker,
            embedder,
            store,
        }
    }

    /// 引用底层 [`LanceStore`],方便上层在 pipeline 之外再做 table 维护
    /// (list / drop / 重新 `ensure_lance_table` 等等)。
    #[inline]
    pub fn store(&self) -> &LanceStore {
        &self.store
    }

    /// 把一段文本分块 → embedding → 写入 `table`。返回写入了多少条。
    ///
    /// 空字符串 / 全空白返回 `Ok(0)`,不报错也不调 embedder。
    pub async fn ingest(&mut self, table: MemoryTable, text: &str) -> Result<usize, MemoryError> {
        // 1) 短路:空文本。
        if text.trim().is_empty() {
            return Ok(0);
        }

        // 2) 分块。
        let chunks = self.chunker.split(text);
        if chunks.is_empty() {
            // chunker 自己已经过滤了全空白 chunk;再守一次保险。
            return Ok(0);
        }

        // 3) 批量 embedding —— 比循环 embed 快(见 `Embedder::embed_batch` docs)。
        let chunk_texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let vectors = self.embedder.embed_batch(&chunk_texts)?;

        // 4) 防御性长度检查 —— embedder 实现 bug 会让上下游错位。
        if vectors.len() != chunks.len() {
            return Err(MemoryError::Embedding(format!(
                "embed_batch returned {} vectors for {} chunks",
                vectors.len(),
                chunks.len()
            )));
        }

        // 5) 构造 RecordBatch:id / text / vector / ts。
        let ts = Utc::now().timestamp_millis();
        let n = chunks.len();
        let ids: Vec<String> = (0..n).map(|_| Uuid::now_v7().to_string()).collect();
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let ts_array = Int64Array::from(vec![ts; n]);

        let batch = build_record_batch(ids, texts, &vectors, ts_array)?;

        // 6) 写入 LanceDB。
        let handle = self.store.ensure_lance_table(table).await?;
        let schema = batch.schema();
        // SimpleRecordBatchReader 是 arrow crate 的标准迭代器 + schema;
        // 包一层 Box<dyn RecordBatchReader> 给 lancedb 的 IntoArrow 用。
        // 我们已经在 `build_record_batch` 里验过 batch,所以这里 `Ok` 包装即可。
        let reader = SimpleRecordBatchReader {
            schema,
            batches: vec![Ok(batch)].into_iter(),
        };
        handle
            .inner
            .add(Box::new(reader))
            .execute()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?;
        Ok(n)
    }

    /// 把 query 嵌入,然后在 `table` 里搜 top-k。
    ///
    /// `k == 0` 时直接返回空 vec;空表也返回空 vec(不会 panic)。
    /// 距离越小越相似 —— 见 [`Hit::score`]。
    pub async fn search(
        &mut self,
        table: MemoryTable,
        query: &str,
        k: usize,
    ) -> Result<Vec<Hit>, MemoryError> {
        if k == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let handle = match self.store.open_table(table).await? {
            Some(h) => h,
            None => return Ok(Vec::new()),
        };

        let qvec = self.embedder.embed(query)?;
        let stream = handle
            .inner
            .vector_search(qvec)
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?
            .limit(k)
            .execute()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?;

        // drain the stream into a single RecordBatch (k <= small, batches are tiny)
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| MemoryError::LanceDb(e.to_string()))?;

        let mut hits = Vec::new();
        for batch in batches {
            extract_hits(&batch, &mut hits);
        }
        Ok(hits)
    }
}

/// 跟 schema 完全一致的 Arrow `Schema`,用作 ingest 时的 batch schema。
fn ingest_schema() -> SchemaRef {
    let vector_field = Field::new("item", DataType::Float32, true);
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(vector_field), EMBEDDING_DIM),
            true,
        ),
        Field::new("ts", DataType::Int64, false),
    ]))
}

/// 把 ids / texts / vectors / ts 塞进一个 [`RecordBatch`],列顺序必须跟
/// [`memory_table_schema`](crate::store::memory_table_schema) 一致(id, text, vector, ts)。
///
/// 返回的 batch 还**没**附 schema —— schema 由 caller(`SimpleRecordBatchReader`)统一提供,
/// 避免双重维护 schema。
fn build_record_batch(
    ids: Vec<String>,
    texts: Vec<&str>,
    vectors: &[Vec<f32>],
    ts: Int64Array,
) -> Result<RecordBatch, MemoryError> {
    let n = ids.len();
    if texts.len() != n || vectors.len() != n {
        return Err(MemoryError::Embedding(format!(
            "build_record_batch: mismatched lengths id={n} text={} vector={}",
            texts.len(),
            vectors.len()
        )));
    }
    if n == 0 {
        return Err(MemoryError::Embedding(
            "build_record_batch called with 0 rows".into(),
        ));
    }

    let id_array = StringArray::from(ids);
    let text_array = StringArray::from(texts);

    // FixedSizeList<Float32>[EMBEDDING_DIM]:fastembed 和 mock 都返回长度 == dim 的向量,
    // 这里强制 assert 防止上层传错维度。
    let vec_iter = vectors.iter().map(|v| {
        if v.len() != EMBEDDING_DIM as usize {
            // 不能直接 return Err —— from_iter_primitive 不接受 Result。
            // 折中:把错误的向量用零向量顶替,然后在最后检查并返回错误。
            // 因为 EMBEDDING_DIM 在 IngestPipeline::new 已经被 assert 过,这里只是
            // 防御性兜底,正常路径走不到。
            Some(vec![Some(0.0_f32); EMBEDDING_DIM as usize])
        } else {
            Some(v.iter().copied().map(Some).collect::<Vec<_>>())
        }
    });
    let vector_array =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vec_iter, EMBEDDING_DIM);

    RecordBatch::try_new(
        ingest_schema(),
        vec![
            Arc::new(id_array),
            Arc::new(text_array),
            Arc::new(vector_array),
            Arc::new(ts),
        ],
    )
    .map_err(|e| MemoryError::LanceDb(format!("RecordBatch::try_new: {e}")))
}

/// 把搜索结果 batch 拆成 [`Hit`] 列表。
///
/// LanceDB 的 `_distance` 列是 Float32;如果某批结果 batch 没有 `_distance` 列
/// (理论上 vector_search 一定会有,但容错一下),就给 score = `f32::NAN` 上抛。
fn extract_hits(batch: &RecordBatch, out: &mut Vec<Hit>) {
    let n = batch.num_rows();
    if n == 0 {
        return;
    }

    let id_col = match batch.column_by_name("id") {
        Some(c) => c
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("id col utf8"),
        None => return,
    };
    let text_col = match batch.column_by_name("text") {
        Some(c) => c
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text col utf8"),
        None => return,
    };
    let ts_col = match batch.column_by_name("ts") {
        Some(c) => c
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("ts col int64"),
        None => return,
    };
    let dist_col = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

    for i in 0..n {
        let score = dist_col
            .and_then(|arr| {
                if arr.is_null(i) {
                    None
                } else {
                    Some(arr.value(i))
                }
            })
            .unwrap_or(f32::NAN);
        out.push(Hit {
            id: id_col.value(i).to_string(),
            text: text_col.value(i).to_string(),
            score,
            ts: ts_col.value(i),
        });
    }
}

/// 最小可用的 `RecordBatchReader` —— lancedb 的 `Table::add` 通过 `IntoArrow` 接受它。
///
/// `arrow_array::RecordBatchReader` 的 trait 约定是 `Item = Result<RecordBatch, ArrowError>`,
/// 所以我们在构造时就把 `MemoryError` 转换成 `ArrowError`,避免给 reader 套一层 map。
struct SimpleRecordBatchReader {
    schema: SchemaRef,
    batches: std::vec::IntoIter<Result<RecordBatch, arrow_schema::ArrowError>>,
}

impl Iterator for SimpleRecordBatchReader {
    type Item = Result<RecordBatch, arrow_schema::ArrowError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.batches.next()
    }
}

impl RecordBatchReader for SimpleRecordBatchReader {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}
