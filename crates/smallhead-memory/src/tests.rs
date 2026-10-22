use tempfile::TempDir;

use crate::{
    Embedder, EmbeddingBackend, EmbeddingProvider, IngestPipeline, IngestSource, LanceStore,
    MemoryConfig, MemoryError, MemoryTable,
};

// ---- async LanceDB e2e (requires lancedb runtime) -----------------------

async fn connected_store() -> (TempDir, LanceStore) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = MemoryConfig::for_root(dir.path().to_path_buf());
    let store = LanceStore::connect(cfg).await.expect("connect");
    (dir, store)
}

// ---- MemoryConfig::default_for_home ----

#[test]
fn default_for_home_returns_expected_fields() {
    let cfg = MemoryConfig::default_for_home().expect("HOME must be set in the test env");

    assert_eq!(cfg.store, EmbeddingProvider::LanceDB);
    assert!(cfg.path.ends_with(".smallhead/memory/lance"));

    assert_eq!(cfg.embedding.provider, EmbeddingBackend::Fastembed);
    assert_eq!(cfg.embedding.model, "bge-small-zh-v1.5");
    assert_eq!(cfg.embedding.dim, 512);

    assert_eq!(cfg.ingest.chunk_size, 512);
    assert_eq!(cfg.ingest.chunk_overlap, 64);
    assert_eq!(cfg.ingest.sources, vec![IngestSource::SessionMessages]);
    assert_eq!(cfg.ingest.retain_days, 0);

    assert!((cfg.search.hybrid_ratio - 0.7).abs() < f32::EPSILON);
    assert_eq!(cfg.search.top_k, 5);
    assert_eq!(cfg.search.recency_decay_days, 0);
}

#[test]
fn default_for_home_validates() {
    let cfg = MemoryConfig::default_for_home().expect("HOME set");
    cfg.validate().expect("defaults must validate");
}

// ---- YAML round-trip ----

#[test]
fn from_yaml_minimal_partial_override() {
    // Only override search.top_k — everything else should fall back to defaults.
    let yaml = "search:\n  top_k: 13\n";
    let cfg = MemoryConfig::from_yaml_str(yaml).expect("parse");

    assert_eq!(cfg.search.top_k, 13);
    // Defaults preserved:
    assert!((cfg.search.hybrid_ratio - 0.7).abs() < f32::EPSILON);
    assert_eq!(cfg.embedding.dim, 512);
}

#[test]
fn from_yaml_full_round_trip() {
    let cfg = MemoryConfig::default_for_home().expect("HOME set");
    let yaml = cfg.to_yaml_string().expect("serialize");
    let back = MemoryConfig::from_yaml_str(&yaml).expect("parse");
    assert_eq!(back.search.top_k, cfg.search.top_k);
    assert_eq!(back.embedding.model, cfg.embedding.model);
    assert_eq!(back.ingest.chunk_overlap, cfg.ingest.chunk_overlap);
    assert_eq!(back.path, cfg.path);
}

#[test]
fn from_yaml_invalid_returns_yaml_err() {
    let bogus = "store: : :\n  - not yaml\n";
    let err = MemoryConfig::from_yaml_str(bogus).expect_err("must fail");
    assert!(matches!(err, MemoryError::Yaml(_)), "got {:?}", err);
}

// ---- validate() ----

#[test]
fn validate_rejects_hybrid_ratio_out_of_range() {
    let mut cfg = MemoryConfig::default_for_home().unwrap();
    cfg.search.hybrid_ratio = 1.5;
    let err = cfg.validate().expect_err("must fail");
    assert!(
        matches!(err, MemoryError::InvalidConfig(_)),
        "got {:?}",
        err
    );
}

#[test]
fn validate_rejects_zero_top_k() {
    let mut cfg = MemoryConfig::default_for_home().unwrap();
    cfg.search.top_k = 0;
    assert!(matches!(
        cfg.validate().expect_err("must fail"),
        MemoryError::InvalidConfig(_)
    ));
}

#[test]
fn validate_rejects_overlap_ge_chunk_size() {
    let mut cfg = MemoryConfig::default_for_home().unwrap();
    cfg.ingest.chunk_overlap = cfg.ingest.chunk_size;
    assert!(matches!(
        cfg.validate().expect_err("must fail"),
        MemoryError::InvalidConfig(_)
    ));
}

#[test]
fn from_yaml_rejects_bad_value_through_validate() {
    let yaml = "search:\n  hybrid_ratio: 2.0\n  top_k: 3\n";
    let err = MemoryConfig::from_yaml_str(yaml).expect_err("must fail");
    assert!(
        matches!(err, MemoryError::InvalidConfig(_)),
        "got {:?}",
        err
    );
}

// ---- MemoryTable ----

#[test]
fn memory_table_dir_names_round_trip() {
    for t in MemoryTable::all() {
        let name = t.dir_name();
        assert_eq!(MemoryTable::from_dir_name(name), Some(*t));
    }
}

#[test]
fn memory_table_unknown_dir_is_none() {
    assert!(MemoryTable::from_dir_name("nope").is_none());
    assert!(MemoryTable::from_dir_name("").is_none());
    assert!(MemoryTable::from_dir_name("AGENT_MEMORY").is_none()); // case-sensitive
}

// ---- LanceStore ----

fn temp_cfg(dir: &TempDir) -> MemoryConfig {
    MemoryConfig::for_root(dir.path().to_path_buf())
}

#[test]
fn store_new_is_empty() {
    let dir = TempDir::new().unwrap();
    let cfg = temp_cfg(&dir);
    let store = LanceStore::new(cfg).expect("create");
    let tables = store.list_tables().expect("list");
    assert!(tables.is_empty(), "got {:?}", tables);
}

#[test]
fn store_ensure_table_then_list() {
    let dir = TempDir::new().unwrap();
    let store = LanceStore::new(temp_cfg(&dir)).unwrap();

    let p = store.ensure_table(MemoryTable::AgentMemory).unwrap();
    assert!(p.exists());
    assert!(p.is_dir());

    let tables = store.list_tables().unwrap();
    assert_eq!(tables, vec![MemoryTable::AgentMemory]);
}

#[test]
fn store_ensure_all_tables() {
    let dir = TempDir::new().unwrap();
    let store = LanceStore::new(temp_cfg(&dir)).unwrap();
    for t in MemoryTable::all() {
        store.ensure_table(*t).unwrap();
    }
    // list_tables returns MemoryTable::all() order.
    assert_eq!(store.list_tables().unwrap(), MemoryTable::all().to_vec());
}

#[test]
fn store_ignores_unknown_subdirs() {
    let dir = TempDir::new().unwrap();
    let store = LanceStore::new(temp_cfg(&dir)).unwrap();
    std::fs::create_dir_all(store.base_path.join("not_a_table")).unwrap();
    std::fs::write(store.base_path.join("stray.txt"), "hi").unwrap();
    assert!(store.list_tables().unwrap().is_empty());
}

#[test]
fn store_table_path_is_under_base() {
    let dir = TempDir::new().unwrap();
    let store = LanceStore::new(temp_cfg(&dir)).unwrap();
    let p = store.table_path(MemoryTable::PersonalRag);
    assert!(p.starts_with(&store.base_path));
    assert!(p.ends_with("personal_rag"));
}

#[tokio::test]
async fn store_real_connect_creates_dir() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    let cfg = MemoryConfig::for_root(path.clone());

    // Sanity: dir should not exist yet (tempfile creates a unique empty dir).
    // Note: some platforms may leave the dir created — we only assert post-connect.
    let store = LanceStore::connect(cfg).await.expect("connect");
    assert!(
        store.base_path.exists(),
        "base_path {} should exist after connect",
        store.base_path.display()
    );
    assert_eq!(store.base_path, path);
}

#[tokio::test]
async fn store_real_ensure_then_open() {
    let (_dir, store) = connected_store().await;

    let handle = store
        .ensure_lance_table(MemoryTable::AgentMemory)
        .await
        .expect("ensure");
    assert_eq!(handle.table, MemoryTable::AgentMemory);
    assert_eq!(handle.inner.name(), MemoryTable::AgentMemory.dir_name());

    let opened = store
        .open_table(MemoryTable::AgentMemory)
        .await
        .expect("open");
    assert!(opened.is_some(), "expected Some after ensure");
}

#[tokio::test]
async fn store_real_list_tables_includes_created() {
    let (_dir, store) = connected_store().await;

    let _ = store
        .ensure_lance_table(MemoryTable::AgentMemory)
        .await
        .expect("ensure AgentMemory");
    let _ = store
        .ensure_lance_table(MemoryTable::PersonalRag)
        .await
        .expect("ensure PersonalRag");

    let names = store.list_lance_tables().await.expect("list");
    assert!(
        names.contains(&MemoryTable::AgentMemory.dir_name().to_string()),
        "expected list to contain agent_memory, got {:?}",
        names
    );
    assert!(
        names.contains(&MemoryTable::PersonalRag.dir_name().to_string()),
        "expected list to contain personal_rag, got {:?}",
        names
    );
}

#[tokio::test]
async fn store_real_drop_table() {
    let (_dir, store) = connected_store().await;
    let _ = store
        .ensure_lance_table(MemoryTable::EcsCheckpoints)
        .await
        .expect("ensure");

    store
        .drop_table(MemoryTable::EcsCheckpoints)
        .await
        .expect("drop");

    let opened = store
        .open_table(MemoryTable::EcsCheckpoints)
        .await
        .expect("open after drop");
    assert!(opened.is_none(), "expected None after drop");
}

// ---- chunker ----

use crate::chunker::{Chunker, DEFAULT_CHUNK_SIZE, DEFAULT_OVERLAP};

#[test]
fn chunker_default_const() {
    assert_eq!(DEFAULT_CHUNK_SIZE, 512);
    assert_eq!(DEFAULT_OVERLAP, 64);
}

#[test]
fn chunker_split_short_text() {
    let c = Chunker::with_defaults();
    let text: String = "你好世界".chars().cycle().take(100).collect(); // 100 char
    let chunks = c.split(&text);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start, 0);
    assert_eq!(chunks[0].end, 100);
    assert_eq!(chunks[0].text, text);
}

#[test]
fn chunker_split_chinese_long() {
    let c = Chunker::with_defaults();
    // 2000 个中文字符。
    let base = "这是一段用于测试的中文文本,用来验证 chunker 能够按照字节正确切分。";
    let text: String = base.chars().cycle().take(2000).collect();
    let chunks = c.split(&text);

    // 每段不应超过 512 chars (允许 boundary 让其略小,但绝不超过)。
    for ck in &chunks {
        let len = ck.text.chars().count();
        assert!(
            len <= DEFAULT_CHUNK_SIZE,
            "chunk len={} > {}: {:?}",
            len,
            DEFAULT_CHUNK_SIZE,
            ck.text.chars().take(40).collect::<String>()
        );
    }

    assert!(
        chunks.len() >= 4,
        "expected >=4 chunks for 2000 chars, got {}",
        chunks.len()
    );

    // 全文覆盖:first.start == 0, last.end == total。
    assert_eq!(chunks.first().unwrap().start, 0);
    assert_eq!(chunks.last().unwrap().end, text.chars().count());

    // 验证:全文被 chunks 完整覆盖,相邻 chunk 重叠区间内容一致。
    let total_chars = text.chars().count();
    assert_eq!(chunks.first().unwrap().start, 0);
    assert_eq!(chunks.last().unwrap().end, total_chars);

    // 相邻 chunk:后段起点 == 前段终点 - overlap,且重叠区间内容一致。
    let overlap = c.overlap();
    for pair in chunks.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        assert_eq!(
            next.start,
            prev.end - overlap,
            "next.start {} != prev.end {} - overlap {}",
            next.start,
            prev.end,
            overlap
        );
        let prev_overlap: String = prev
            .text
            .chars()
            .rev()
            .take(overlap)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let next_overlap: String = next.text.chars().take(overlap).collect();
        assert_eq!(
            prev_overlap, next_overlap,
            "overlap region content mismatch"
        );
    }
}

#[test]
fn chunker_split_empty() {
    let c = Chunker::with_defaults();
    assert!(c.split("").is_empty());
}

#[test]
fn chunker_overlap_correctness() {
    // 用小 chunk_size 让切出多个 chunk,验证 overlap 区域严格相等。
    let c = Chunker::new(20, 5);
    let base = "abcdefghijklmnopqrstuvwxyz0123456789"; // 36 chars
    let text = base.repeat(3); // 108 chars
    let chunks = c.split(&text);
    assert!(
        chunks.len() >= 2,
        "need >=2 chunks for overlap test, got {}",
        chunks.len()
    );

    // 相邻 chunk 的 overlap 区域: chunk[i].text 末 5 chars == chunk[i+1].text 首 5 chars。
    let overlap = c.overlap();
    for pair in chunks.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        let prev_tail: String = prev
            .text
            .chars()
            .rev()
            .take(overlap)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let next_head: String = next.text.chars().take(overlap).collect();
        assert_eq!(
            prev_tail, next_head,
            "overlap mismatch between {:?} and {:?}",
            prev.text, next.text
        );
    }
}

#[test]
fn chunker_boundary_preferred() {
    // 2 句话,如果 chunk_size 装得下且 boundary 逻辑生效就能切开。
    // 没有 boundary 逻辑会被硬切成 1 个 chunk。
    // 选 chunk_size=7:第一句 6 char + 句号 6 char = 12 char。hard_end=7,
    // 最后一个 boundary 在 idx 5 处(dist=1, ≤ chunk_size/4=1),所以切到 idx 6。
    // overlap=0 让两个 chunk 文本可严格断言。
    let c = Chunker::new(7, 0);
    let text = "这是第一句。这是第二句。"; // 12 chars
    let chunks = c.split(text);
    assert_eq!(
        chunks.len(),
        2,
        "expected 2 chunks at boundary, got {}: {:?}",
        chunks.len(),
        chunks.iter().map(|x| x.text.clone()).collect::<Vec<_>>()
    );
    assert_eq!(chunks[0].text, "这是第一句。");
    assert_eq!(chunks[1].text, "这是第二句。");
}

#[test]
#[should_panic(expected = "overlap")]
fn chunker_assert_overlap_lt_chunk() {
    let _ = Chunker::new(100, 200);
}

// ---- embedding (LocalEmbedder) ----
//
// The two `#[ignore]` tests below download ~50MB of model weights + the
// ONNX runtime shared library on first run. They are gated out of CI;
// run manually with:
//
//     cargo test -p smallhead-memory -- --ignored embed_
//

#[test]
fn embed_const_dim_is_512() {
    // Pure constant check — must always pass, no network, no LTO.
    assert_eq!(crate::BGE_SMALL_ZH_DIM, 512);
}

#[test]
#[ignore = "downloads ~50MB model + ONNX runtime; run with --ignored"]
fn embed_smoke() {
    let mut embedder = crate::LocalEmbedder::new().expect("init bge-small-zh-v1.5");
    assert_eq!(embedder.dim(), 512);

    let v = embedder.embed("你好世界").expect("embed single");
    assert_eq!(v.len(), 512);
    assert!(
        v.iter().any(|x| *x != 0.0),
        "embedding is all zeros: len={}",
        v.len()
    );
}

#[test]
#[ignore = "downloads ~50MB model + ONNX runtime; run with --ignored"]
fn embed_batch_shape() {
    let mut embedder = crate::LocalEmbedder::new().expect("init bge-small-zh-v1.5");
    let docs: [&str; 2] = ["a", "b"];
    let out = embedder.embed_batch(&docs).expect("embed batch");
    assert_eq!(out.len(), 2);
    for v in &out {
        assert_eq!(v.len(), 512);
        assert!(v.iter().any(|x| *x != 0.0));
    }
}

// ---- pipeline e2e (D-team ingest + search) ----
//
// 这组测试必须 **不依赖网络 / 不下载 bge-small-zh 模型**,所以用一个 deterministic
// 的 mock embedder:对每个 char 取 u32 code,扩展到 dim 维并 L2 normalize。
// LanceDB vector search 喜欢非零向量(全零会让 distance 全 0,排名退化),
// normalize 之后 cosine / L2 排名一致 —— 测试 "search 回自己" 就能稳定命中 source。

use crate::store::EMBEDDING_DIM;

/// Hash-style deterministic mock embedder,维度固定 [`EMBEDDING_DIM`]。
///
/// 把 `text.chars()` 头 `dim` 个的 `u32` 值除以 65535 填到向量里,然后 L2 normalize。
/// 同一个文本 → 同一个向量;不同文本 → 几乎一定不同(除非前 dim 个 chars 完全一样)。
struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    fn new() -> Self {
        Self {
            dim: EMBEDDING_DIM as usize,
        }
    }
}

impl Embedder for MockEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&mut self, text: &str) -> Result<Vec<f32>, MemoryError> {
        // 用一个比纯 zero 略好,但依然 deterministic 的填充方案:
        //   1) 把 text 的每个 char 的 u32 code 散列到 [0, 1);
        //   2) 填到前 `min(text.len(), dim)` 个位置;
        //   3) 剩下的位置用一个简单的 LCG 填充,保证不同长度也至少有点区分度;
        //   4) L2 normalize —— 这样 cosine / L2 排名一致,search 自匹配稳。
        let mut v = vec![0.0f32; self.dim];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15; // splitmix64 常数,任意选一个
        for (i, slot) in v.iter_mut().enumerate() {
            // splitmix64 step
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let frac = (z as f64) / (u64::MAX as f64);
            // 用 char code 叠一个修正项,让相同前缀的文本也有区分度
            let char_mix = text
                .chars()
                .nth(i % text.chars().count().max(1))
                .map(|c| (c as u32 as f64) / 65535.0)
                .unwrap_or(0.0);
            *slot = ((frac + char_mix) * 0.5) as f32;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in v.iter_mut() {
            *x /= norm;
        }
        Ok(v)
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

async fn connected_pipeline() -> (TempDir, IngestPipeline<MockEmbedder>) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = MemoryConfig::for_root(dir.path().to_path_buf());
    let store = LanceStore::connect(cfg).await.expect("connect");
    let pipeline = IngestPipeline::new(Chunker::with_defaults(), MockEmbedder::new(), store);
    (dir, pipeline)
}

#[tokio::test]
async fn pipeline_ingest_returns_chunk_count() {
    let (_dir, mut p) = connected_pipeline().await;

    // 2000 chars 中文,defaults 是 chunk_size=512,overlap=64 → 期望 >= 4 个 chunk。
    let base = "这是一段用于测试的中文文本,用来验证 chunker 能够按照字节正确切分。";
    let text: String = base.chars().cycle().take(2000).collect();

    let n = p
        .ingest(MemoryTable::AgentMemory, &text)
        .await
        .expect("ingest");
    let expected = Chunker::with_defaults().split(&text).len();
    assert_eq!(
        n, expected,
        "ingest returned {n}, expected chunker.split len = {expected}"
    );
}

#[tokio::test]
async fn pipeline_search_returns_top_k() {
    let (_dir, mut p) = connected_pipeline().await;

    // 灌 5 条不重复的短文,确保 search 不会因为 chunker 切成多块而搞乱计数。
    let docs = [
        "alpha: quick brown fox",
        "beta: lazy dog sleeps",
        "gamma: hello world",
        "delta: rust async await",
        "epsilon: lancedb vector search",
    ];
    for d in docs {
        p.ingest(MemoryTable::PersonalRag, d).await.expect("ingest");
    }

    let hits = p
        .search(MemoryTable::PersonalRag, "alpha", 3)
        .await
        .expect("search");
    assert!(hits.len() <= 3, "got {} hits, expected <= 3", hits.len());
    assert!(
        !hits.is_empty(),
        "expected at least one hit for non-empty table"
    );

    for h in &hits {
        assert!(
            h.score.is_finite(),
            "score should be finite, got {}",
            h.score
        );
        // uuid-v7 长度固定 36 (8-4-4-4-12)
        assert_eq!(h.id.len(), 36, "id is not a uuid-v7 string: {:?}", h.id);
        assert!(h.ts > 0, "ts should be positive millis since epoch");
    }
}

#[tokio::test]
async fn pipeline_search_self_match_highest_score() {
    let (_dir, mut p) = connected_pipeline().await;

    // 灌三段区分度大的文本,然后用第一段当 query,期望 top hit 文本 = 第一段。
    let docs = [
        "the quick brown fox jumps over the lazy dog",
        "machine learning models transform input features into predictions",
        "rust async runtime schedules tasks across multiple worker threads",
    ];
    for d in docs {
        p.ingest(MemoryTable::AgentMemory, d).await.expect("ingest");
    }

    let query = docs[0];
    let hits = p
        .search(MemoryTable::AgentMemory, query, 1)
        .await
        .expect("search");
    assert_eq!(
        hits.len(),
        1,
        "expected exactly 1 hit with k=1, got {}",
        hits.len()
    );
    // Mock embedder 是 deterministic 的 —— 同一段文本向量完全一致,top hit
    // 必须是源文本。
    assert_eq!(
        hits[0].text, query,
        "top hit should be the source text, got: {:?}",
        hits[0].text
    );
    assert!(hits[0].score.is_finite() && hits[0].score >= 0.0);
}

#[tokio::test]
async fn pipeline_ingest_empty_text_is_noop() {
    let (_dir, mut p) = connected_pipeline().await;

    assert_eq!(
        p.ingest(MemoryTable::AgentMemory, "").await.expect("empty"),
        0
    );
    assert_eq!(
        p.ingest(MemoryTable::AgentMemory, "   \n\t  ")
            .await
            .expect("whitespace"),
        0
    );

    // 表还没创建 → search 应该返回空 vec,不能 panic。
    let hits = p
        .search(MemoryTable::AgentMemory, "anything", 5)
        .await
        .expect("search on missing table");
    assert!(hits.is_empty(), "expected empty vec, got {:?}", hits);
}

#[tokio::test]
async fn pipeline_search_before_ingest_returns_empty() {
    let (_dir, mut p) = connected_pipeline().await;

    // 直接 search —— 表都还没创建。
    let hits = p
        .search(MemoryTable::AgentMemory, "rust async", 5)
        .await
        .expect("search");
    assert!(
        hits.is_empty(),
        "expected empty vec before any ingest, got {:?}",
        hits
    );
}
