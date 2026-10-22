# 04 — Session Storage

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-session/src/`
> 上游: `ARCHITECTURE.md` §7, §8; `AGENT_METHODOLOGY.md` §1.4, §1.6

---

## 1. 概述

Session 是 smallhead 的真理来源：不可变 JSONL 事件流 + SQLite 索引。设计哲学继承自四个项目的共识：

- **append-only**：原始 entries 永不修改、永不删除。Compaction 是新事件，不是修改。
- **JSONL 是真理来源，SQLite 是索引**：SQLite 崩溃可从 JSONL 重建。
- **first-kept-entry-id 是 compaction 边界**：从该 entry 起，原始消息开始保留；之前的消息被 `summary` 替代。
- **anchor_refs 是摘要可验证性的关键不变量**（来自 big-head 教训）：compaction entry 必须保留被引用 entry IDs。

big-head 用 v3 binary（`checkpoint.json` + `checkpoint.blobs` + `checkpoint.journal`），对小数据是过度设计。smallhead 直接 JSONL，1MB 以下无差别。

---

## 2. 目录布局

```
~/.local/share/smallhead/sessions/
└── {cwd-hash}/                          # cwd 的 SHA-256 前 16 字符
    └── {session-id}/                    # UUID v7
        ├── entries.jsonl                # append-only 事件流（真理来源）
        ├── checkpoints/                 # ECS world snapshots (sparse)
        │   └── {version}.json
        ├── meta.sqlite                  # SQLite 索引（best-effort）
        └── attachments/                 # tool outputs > N tokens
            └── {file-id}.bin
```

- `{cwd-hash}`：cwd 的 SHA-256 前 16 字符（hex）—— 防止路径遍历、便于聚合。
- `{session-id}`：UUID v7（时间序）—— 自带时间排序能力。

---

## 3. Entry Schema

```rust
// smallhead-session/src/entry.rs

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,                            // UUID v7
    pub parent_id: Option<String>,             // 形成 tree（branch / fork）
    pub timestamp: i64,                        // ms since epoch
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    Message(AgentMessage),
    Compaction {
        first_kept_entry_id: String,
        summary: String,
        anchor_refs: Vec<AnchorRef>,
    },
    BranchMarker {
        label: String,
    },
    ProviderTurn {
        request_hash: String,                  // sha256 of ProviderRequest
        events: Vec<ProviderEvent>,            // 完整流（重放用）
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        output_summary: String,                // 完整 output 在 attachments/
        full_output_path: Option<String>,
    },
    Checkpoint {
        version: u64,
        world_snapshot_path: String,           // checkpoints/{version}.json
    },
    WorldMutation {
        entity_id: u64,
        component: String,
        before: serde_json::Value,
        after: serde_json::Value,
        reason: String,                        // "tool_call:abc-123" 之类的因果链
    },
    DoomLoopTriggered {
        tool: String,
        count: usize,
    },
    PermissionDecision {
        tool: String,
        action: String,                        // "allow" | "ask" | "deny"
        decided_by: String,                    // "ruleset" | "user"
    },
    Error {
        kind: String,
        message: String,
        recoverable: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorRef {
    pub entry_id: String,
    pub reason: String,                        // "explicit_citation" | "tool_result_referenced" | ...
}
```

---

## 4. JSONL 写入策略

```rust
// smallhead-session/src/writer.rs

pub struct JsonlWriter {
    path: PathBuf,
    pending: Vec<Entry>,
    last_sync: Instant,
    sync_interval: Duration,    // 默认 5s
    sync_threshold: usize,      // 默认 5 条
}

impl JsonlWriter {
    pub async fn append(&mut self, entry: Entry) -> Result<()> {
        self.pending.push(entry);
        if self.pending.len() >= self.sync_threshold
            || self.last_sync.elapsed() >= self.sync_interval
        {
            self.sync().await?;
        }
        Ok(())
    }

    pub async fn sync(&mut self) -> Result<()> {
        if self.pending.is_empty() { return Ok(()); }

        // 1. 追加到 entries.jsonl
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .await?;
        for entry in &self.pending {
            let bytes = serde_json::to_vec(entry)?;
            file.write_all(&bytes).await?;
            file.write_all(b"\n").await?;
        }

        // 2. fsync（5 条批量 = amortized cost 低）
        file.sync_all().await?;
        drop(file);

        // 3. 异步更新 SQLite（best-effort，不阻塞主路径）
        let entries = std::mem::take(&mut self.pending);
        let sqlite_path = self.sqlite_path.clone();
        tokio::spawn(async move {
            if let Err(e) = update_sqlite(&sqlite_path, &entries).await {
                warn!("SQLite update failed (recoverable from JSONL): {e}");
            }
        });

        self.last_sync = Instant::now();
        Ok(())
    }

    /// 进程退出时强制 sync（hook 触发）
    pub async fn flush_on_exit(&mut self) -> Result<()> {
        self.sync().await
    }
}
```

**关键决策**：
- **5 条批量 fsync**：每次 5ms × 200 fsync/s = amortized 极低，但单次崩溃丢失 ≤ 5 条 entry。
- **SQLite 是 best-effort**：崩溃后启动时从 JSONL 重建。
- **append-only**：永不 truncate / 覆盖 entries.jsonl。

---

## 5. SQLite 索引

```sql
-- smallhead-session/src/schema.sql

CREATE TABLE entries (
    id TEXT PRIMARY KEY,
    parent_id TEXT REFERENCES entries(id),
    timestamp INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload JSON NOT NULL
);

CREATE INDEX idx_parent ON entries(parent_id);
CREATE INDEX idx_timestamp ON entries(timestamp);
CREATE INDEX idx_kind ON entries(kind);

-- 全文搜索（FTS5）
CREATE VIRTUAL TABLE entries_fts USING fts5(id, content);

-- 触发器：插入 entries 时自动索引 content
CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(id, content)
    VALUES (new.id, json_extract(new.payload, '$.content'));
END;
```

**用途**：
- `entries` 表：按 id / parent / 时间 / kind 查询。
- `entries_fts`：FTS5 全文搜索（用户搜 "what did I ask about X yesterday"）。
- **JSONL 仍是真理**：SQLite 损坏 / 不一致时，删除 `meta.sqlite` 启动时从 JSONL 重建。

---

## 6. Branch Tree

`parent_id` 形成 tree，支持多分支：

```
entry-001 (User: "fix auth")
  └── entry-002 (Assistant: tool_calls=[bash])
        └── entry-003 (ToolResult)
              └── entry-004 (Assistant: "fixed")
                    ├── entry-005 (BranchMarker: "alternate approach")
                    │     └── entry-006 (User: "try without caching")
                    │           └── entry-007 (Assistant: ...)
                    └── entry-008 (Compaction: summary, first_kept=004, anchor_refs=[001,003])
```

**`smallhead tree {session-id}`** CLI 命令可视化为：

```
* 004 Assistant "fixed"
│
├── * 008 Compaction (summary, anchors=[001,003])
│
└── * 005 BranchMarker "alternate approach"
      └── * 006 User "try without caching"
```

---

## 7. Compaction（5 步算法，pi 风格）

```rust
// smallhead-session/src/compactor.rs

pub struct Compactor {
    pub context_window: usize,
    pub reserve_tokens: usize,           // 默认 16,384
    pub summary_model: String,           // 默认 fast model (haiku)
    pub provider: Arc<dyn Provider>,
}

impl Compactor {
    pub async fn compact(&self, session: &SessionStore) -> Result<CompactionEntry> {
        let entries = session.all_entries().await?;
        let total_tokens = self.estimate_tokens(&entries);
        if total_tokens < self.context_window - self.reserve_tokens {
            return Ok(CompactionEntry::noop());  // no-op
        }

        // 1. Walk backwards 找到 cut point
        let target = total_tokens - (self.context_window - self.reserve_tokens);
        let cut = self.find_cut_point(&entries, target);

        // 2. Extract anchor refs（被引用的 entry IDs）
        let anchor_refs = self.extract_anchors(&entries[..cut]);

        // 3. 调用 LLM 摘要（保留 anchor refs 上下文）
        let summary = self.summarize(&entries[..cut], &anchor_refs).await?;

        // 4. Append CompactionEntry（永不修改 entries[..cut]）
        let comp = CompactionEntry {
            entry_id: Uuid::now_v7().to_string(),
            first_kept_entry_id: entries[cut].id.clone(),
            summary,
            anchor_refs,
        };
        session.append(EntryKind::Compaction(comp.clone())).await?;

        // 5. 重建 prompt（按需在 transform_context() 中完成）
        Ok(comp)
    }

    fn find_cut_point(&self, entries: &[Entry], target_tokens: usize) -> usize {
        let mut acc = 0;
        for (i, e) in entries.iter().rev().enumerate() {
            acc += self.estimate_entry_tokens(e);
            if acc >= target_tokens {
                return entries.len() - i;
            }
        }
        0
    }

    fn extract_anchors(&self, entries: &[Entry]) -> Vec<AnchorRef> {
        // 扫描 entries，找到被引用的 entry IDs（tool_call_id 匹配、用户消息的 @mention 等）
        // 这是 smallhead 区别于其他项目的关键不变量
        let mut refs = Vec::new();
        for entry in entries {
            if let Some(referenced) = self.scan_for_references(entry) {
                for id in referenced {
                    refs.push(AnchorRef {
                        entry_id: id,
                        reason: self.classify_reference(entry, &id),
                    });
                }
            }
        }
        refs.sort_by_key(|r| r.entry_id.clone());
        refs.dedup_by(|a, b| a.entry_id == b.entry_id);
        refs
    }

    async fn summarize(&self, entries: &[Entry], anchors: &[AnchorRef]) -> Result<String> {
        // 调用 fast model，prompt 包含：
        // - entries[..cut] 的摘要任务
        // - 显式要求保留 anchor refs 涉及的 detail
        // - 提示"如果 anchor 涉及的事实变了，明确指出"
        todo!()
    }
}
```

**关键不变量（来自 big-head 教训）**：
- 原始 entries 永不删除——compaction 是新 entry。
- CompactionEntry 必须包含 `anchor_refs`（被引用 entry IDs）—— 这样**摘要可独立验证**。
- 测试：写一个"还原测试"——固定输入 → 实际循环 → 独立 reader 读取临时文件 → 验证还原语义一致。

---

## 8. Anchor References（smallhead 关键不变量）

这是 smallhead 与其他 agent 项目的核心差异化：

**问题**：LLM 摘要可能"歪曲原话"——把用户的"用 TypeScript 实现" 总结成"用 JavaScript 实现"。如果摘要不能反查原话，agent 就会基于错误事实继续工作。

**smallhead 的解**：
1. `CompactionEntry` 强制包含 `anchor_refs: Vec<AnchorRef>`——被摘要"涉及"的 entry IDs。
2. 还原时（`transform_context()` 遇到 CompactionEntry）：

   ```rust
   pub fn transform_context(entries: &[Entry]) -> Vec<AgentMessage> {
       let mut msgs = Vec::new();
       let mut kept_from: Option<usize> = None;
       let mut compaction: Option<&Compaction> = None;

       for (i, e) in entries.iter().enumerate() {
           if let EntryKind::Compaction { first_kept_entry_id, summary, anchor_refs } = &e.kind {
               kept_from = entries.iter().position(|x| &x.id == first_kept_entry_id);
               compaction = Some(...);
           }
           if kept_from.is_some_and(|cut| i < cut) {
               continue;  // 跳过被压缩的 entries
           }
           msgs.push(into_message(e));
       }

       // 在 summary block 后注入 anchor 引用提示
       if let Some(anchor_refs) = compaction.anchor_refs {
           msgs.push(AgentMessage::Notification {
               kind: "anchor_refs".into(),
               payload: json!({ "entry_ids": anchor_refs }),
           });
       }

       msgs
   }
   ```

3. **测试**：还原后，把 summary 中提到的事实逐一映射回 entry IDs——如果某条事实找不到 anchor，就是漏 anchor（测试 fail）。

---

## 9. 还原测试（核心）

```rust
// smallhead-session/tests/compaction_round_trip.rs

#[tokio::test]
async fn compaction_round_trip_preserves_anchors() {
    // 1. 准备固定输入序列
    let entries = fixture::sample_conversation();  // 100 entries
    let tempdir = tempdir().unwrap();
    let session = SessionStore::new(tempdir.path()).await.unwrap();
    for entry in &entries {
        session.append(entry.clone()).await.unwrap();
    }

    // 2. 实际循环
    let compactor = Compactor::default();
    let compaction = compactor.compact(&session).await.unwrap();

    // 3. 独立 reader 读取临时文件（不通过 SessionStore API）
    let reader = IndependentReader::new(tempdir.path());
    let summary_block = reader.read_entry(&compaction.entry_id).unwrap();

    // 4. 验证 anchor_refs 覆盖所有被引用的 entry
    let referenced_ids = extract_referenced_ids(&entries);
    assert!(compaction.anchor_refs.iter().all(|r| referenced_ids.contains(&r.entry_id)));

    // 5. 语义还原验证：从 first_kept_entry_id + summary 重建 context
    let restored = compactor.restore_for_prompt(&session).await.unwrap();
    let original_semantic_hash = sha256(extract_semantic(&entries[compaction.cut..]));
    let restored_semantic_hash = sha256(extract_semantic(&restored));
    assert_eq!(original_semantic_hash, restored_semantic_hash,
               "compaction must preserve semantic equivalence");
}
```

**这条测试是 smallhead 的"立身之本"**——直接对应 big-head 的核心教训（来自 `big-head/docs/agent-harness-source-study-20261004.md`）。

---

## 10. Durable Layer（可选 opt-in）

默认 **不启用**——PI 哲学：简单第一。启用条件：

```bash
smallhead run --durable "fix the bug"
```

开启后：
- `~/.local/share/smallhead/inbox/{session-id}/` 持久化 admission inbox
- 每个 tool 启动前先把 intent 写入 inbox
- 进程崩溃后启动时扫描 inbox，重放未完成的 tool

详见 `ARCHITECTURE.md` §8 与未来的 `specs/08-durable-layer.md`。

---

## 11. 与 big-head 对照

| 维度 | big-head | smallhead |
|------|---------|-----------|
| 主格式 | v3 binary (`checkpoint.json` + `checkpoint.blobs` + `checkpoint.journal`) | JSONL + 稀疏 checkpoint |
| 索引 | ECS 自带 world snapshot 索引 | SQLite + FTS5 |
| Compaction | 一次性警告 + early return（未实现自动 compact） | pi 5 步算法 + anchor_refs |
| Branch | 多 World ID（文件系统隔离） | `parent_id` 树 |
| 还原验证 | 无显式语义等价测试 | `compaction_round_trip_preserves_anchors` 测试是 P0 |

**为什么 smallhead 直接 JSONL**：
- 单 session 通常 < 1MB——binary 收益微小。
- JSONL 可读、可 diff、可 grep（开发者体验）。
- v3 binary 是 big-head 为大数据优化——但 smallhead v0.1 先把 90% 场景跑通。

---

## 12. 验收测试

**用例 1: JSONL round-trip**

```rust
#[tokio::test]
async fn jsonl_round_trip_preserves_all_entries() {
    let tempdir = tempdir().unwrap();
    let mut writer = JsonlWriter::new(tempdir.path()).await.unwrap();
    for entry in fixture::sample_entries() {
        writer.append(entry).await.unwrap();
    }
    writer.flush_on_exit().await.unwrap();

    let reader = IndependentReader::new(tempdir.path());
    let read_entries = reader.read_all_entries().await.unwrap();
    assert_eq!(read_entries, fixture::sample_entries());
}
```

**用例 2: SQLite 重建**

```rust
#[tokio::test]
async fn sqlite_rebuilds_from_jsonl() {
    // GIVEN: 只有 entries.jsonl，没有 meta.sqlite
    let tempdir = tempdir().unwrap();
    let mut writer = JsonlWriter::new(tempdir.path()).await.unwrap();
    for entry in fixture::sample_entries() {
        writer.append(entry).await.unwrap();
    }
    writer.flush_on_exit().await.unwrap();

    // WHEN: 删除 SQLite
    tokio::fs::remove_file(tempdir.path().join("meta.sqlite")).await.unwrap();

    // THEN: 启动时自动重建
    let store = SessionStore::new(tempdir.path()).await.unwrap();
    let count = store.count_entries().await.unwrap();
    assert_eq!(count, fixture::sample_entries().len());
}
```

**用例 3: Branch tree 重建**

```rust
#[tokio::test]
async fn branch_tree_query() {
    // GIVEN: 一条带 branch 的 entries 序列
    // WHEN: 查询给定 id 的所有 children
    // THEN: 返回正确子节点（包括 BranchMarker 后的分支）
}
```

**用例 4: Compaction anchor 验证**（见 §9）

---

## 13. 不变量

1. **`entries.jsonl` 永不修改、永远 append**——compaction 是新 entry。
2. **SQLite 是 best-effort**——崩溃可从 JSONL 重建。
3. **每 5 条 entry fsync**——单次崩溃丢失 ≤ 5 条。
4. **CompactionEntry 必须包含 anchor_refs**——测试 fail if missing。
5. **first_kept_entry_id 必须指向 JSONL 中真实存在的 entry**——孤儿 ID 是数据腐化。
6. **UUID v7 严格递增**——保证事件因果顺序。
7. **attachments/ 引用永不被 GC**——只有用户显式 `smallhead gc` 才清理。
8. **branch tree 通过 parent_id 形成**——不允许 cycle。
9. **transform_context() 遇到 CompactionEntry 时**——跳过 first_kept_entry_id 之前的 entries，但 summary 块本身进入 prompt。

---

## 14. 引用

- 上游 spec: `specs/01-message-model.md`（`AgentMessage` / `AnchorRef`）
- 引用本 spec:
  - `specs/02-agent-loop.md` — `transform_context()` 调用
  - `specs/06-ecs-kernel.md`（未来）— `Checkpoint` entry
- 源码: `ARCHITECTURE.md` §7, §8
- 方法论: `AGENT_METHODOLOGY.md` §1.4, §1.6, §4

---

## 15. 变更日志

- **2026-10-04 v0.1** — 初版。JSONL + SQLite 索引 + pi 5 步 compaction + anchor_refs 不变量。
- **未来**: 考虑把 SQLite 索引改为 CRDT（pi-durable 风格）支持多端同步。
