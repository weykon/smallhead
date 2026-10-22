# 08 — Durable Layer

> 状态：v0.1 — 2026-10-04
> Crate: `smallhead-durable/`（opt-in feature flag）
> 上游: `ARCHITECTURE.md` §8; `AGENT_METHODOLOGY.md` §1.6
> 对照: opencode V2 `session_input`、pi-durable `spec.md`

---

## 1. 概述

Durable Layer 让 smallhead **进程崩溃时不丢工作**。设计来源是 opencode V2 Session 模型 + pi-durable 的原子提交原则。

**关键哲学**：默认 **不启用**。Pi 哲学："简单第一"。Durable Layer 引入额外复杂度（inbox / history 分离、atomic commit、resume 协议）只在必要时付出。

启用条件：
- **CI / 无人值守场景**——进程死了不能丢工作。
- **长会话**（> 1h 持续 turn）——checkpoint / fsync 不足以保证不丢工作。
- **用户主动 opt-in**：`smallhead --durable` CLI flag 或 `smallhead.toml` 配置 `durable.enabled = true`。

---

## 2. 核心抽象

```rust
// smallhead-durable/src/session.rs

use std::path::PathBuf;
use tokio::sync::mpsc;
use uuid::Uuid;

pub struct DurableSession {
    inbox: PathBuf,                              // 不可变 admission inbox
    history: PathBuf,                            // 不可变 history
    checkpoints: PathBuf,                        // ECS snapshots（ECS kernel 写入）
    inbox_writer: tokio::sync::Mutex<tokio::fs::File>,
    history_writer: tokio::sync::Mutex<tokio::fs::File>,
    pub session_id: String,
    pub input_tx: mpsc::Sender<SessionInput>,
    pub input_rx: tokio::sync::Mutex<mpsc::Receiver<SessionInput>>,
}

pub enum SessionInput {
    Prompt {
        id: String,
        content: AgentMessage,
        delivery: Delivery,
        resume: bool,
    },
    /// 模型选择变更
    SwitchModel { model: String },
    /// 用户主动中断
    Cancel,
    /// 重启信号（process 启动时由 resume() 触发）
    Resume,
}

pub enum Delivery {
    /// 等当前 turn 完成
    Steer,
    /// 等当前 turn + follow-up 都完成
    FollowUp,
    /// 立刻投递（打断当前 turn）
    Interrupt,
}

pub enum CommitReceipt {
    Committed { session_id: String, last_entry_id: String },
    Failed { reason: String },
}

pub struct ResumedSession {
    pub entries: Vec<Entry>,
    pub last_entry_id: Option<String>,
    pub pending_inputs: Vec<SessionInput>,
    pub interrupted_entries: Vec<Entry>,    // 启动时被标记为 interrupted 的 entry
}
```

**关键概念**：
- **Inbox 是不可变 admission queue**：外部输入（user prompt、switch model、cancel）按到达顺序持久化。
- **History 是不可变事实流**：agent loop 产生的 entries（commit_atomic 提交）。
- **Inbox 与 History 物理分离**：inbox 是 admission，history 是结果。

---

## 3. SessionInput 枚举语义

```rust
// 在 smallhead-durable/src/input.rs

impl SessionInput {
    pub async fn persist(&self, writer: &mut tokio::fs::File) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        writer.write_all(&bytes).await?;
        writer.write_all(b"\n").await?;
        writer.sync_all().await?;        // 每条 inbox entry 立即 fsync
        Ok(())
    }

    pub fn from_entry(entry: &Entry) -> Option<Self> {
        // 从 JSONL 还原
        match &entry.kind {
            EntryKind::Prompt { content, delivery, resume } => Some(Self::Prompt {
                id: entry.id.clone(),
                content: content.clone(),
                delivery: *delivery,
                resume: *resume,
            }),
            EntryKind::SwitchModel { model } => Some(Self::SwitchModel { model: model.clone() }),
            EntryKind::Cancel => Some(Self::Cancel),
            EntryKind::Resume => Some(Self::Resume),
            _ => None,
        }
    }
}
```

**Delivery 语义**：
- `Steer`：插入到 agent loop 的 steering channel（详见 specs/02 §5.3）。
- `FollowUp`：插入到 follow-up 队列（staff 详见 specs/02 §5.3）。
- `Interrupt`：取消当前 tool execution（详见 specs/02 §8 cancellation）。

---

## 4. Atomic Commit 协议

```rust
// smallhead-durable/src/commit.rs

use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

impl DurableSession {
    /// 原子提交一组 entry —— 写 tmp → fsync → rename
    pub async fn commit_atomic(
        &self,
        entries: Vec<Entry>,
    ) -> Result<CommitReceipt, CommitError> {
        if entries.is_empty() {
            return Err(CommitError::Empty);
        }

        // 1. 写临时文件
        let tmp = self.history.with_extension("jsonl.tmp");
        {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&tmp)
                .await?;
            for entry in &entries {
                let bytes = serde_json::to_vec(entry)?;
                f.write_all(&bytes).await?;
                f.write_all(b"\n").await?;
            }
            f.sync_all().await?;          // fsync before rename
            drop(f);
        }

        // 2. 原子 rename
        fs::rename(&tmp, &self.history).await?;

        // 3. 同步 SQLite（best-effort）
        let history_path = self.history.clone();
        let entries_clone = entries.clone();
        tokio::spawn(async move {
            if let Err(e) = update_index(&history_path, &entries_clone).await {
                warn!("SQLite update failed (recoverable from JSONL): {e}");
            }
        });

        Ok(CommitReceipt::Committed {
            session_id: self.session_id.clone(),
            last_entry_id: entries.last().unwrap().id.clone(),
        })
    }

    /// 重启时拾取 —— 标记上次遗留的 running 状态为 interrupted
    pub async fn resume(&self) -> Result<ResumedSession, ResumeError> {
        // 1. 读取所有 entries
        let entries = self.read_all_entries().await?;

        // 2. 找到最后一个 running 状态
        let last_running = entries.iter().rev().find(|e| matches!(
            e.kind,
            EntryKind::ProviderTurn { .. } | EntryKind::ToolCall { .. }
        ));

        let mut interrupted_entries = vec![];
        if let Some(entry) = last_running {
            // 3. append interrupted marker
            let interrupted = Entry {
                id: Uuid::now_v7().to_string(),
                parent_id: Some(entry.id.clone()),
                timestamp: now_ms(),
                kind: EntryKind::Error {
                    kind: "interrupted".into(),
                    message: "tool/provider was running when process died".into(),
                    recoverable: true,
                },
            };
            self.commit_atomic(vec![interrupted.clone()]).await?;
            interrupted_entries.push(interrupted);
        }

        // 4. 读取 inbox（未处理的 input）
        let pending_inputs = self.read_inbox().await?
            .into_iter()
            .filter_map(|e| SessionInput::from_entry(&e))
            .collect();

        Ok(ResumedSession {
            entries,
            last_entry_id: entries.last().map(|e| e.id.clone()),
            pending_inputs,
            interrupted_entries,
        })
    }

    async fn read_all_entries(&self) -> Result<Vec<Entry>, std::io::Error> {
        let content = tokio::fs::read_to_string(&self.history).await?;
        content.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| e.into()))
            .collect()
    }

    async fn read_inbox(&self) -> Result<Vec<Entry>, std::io::Error> {
        let path = self.inbox.with_extension("jsonl");
        if !path.exists() { return Ok(vec![]); }
        let content = tokio::fs::read_to_string(&path).await?;
        content.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| e.into()))
            .collect()
    }
}
```

**为什么 tmp → fsync → rename**：
1. **崩溃一致性**：写一半时进程被 kill，tmp 文件留在原地，history.jsonl 保留旧版本。
2. **POSIX rename atomic**：POSIX 保证 `rename` 在同文件系统上是 atomic 的。

---

## 5. 持久化目录布局

```
~/.local/share/smallhead/inbox/
└── {session-id}/                     # UUID v7
    ├── inbox.jsonl                   # 不可变 admission queue
    ├── history.jsonl                 # 不可变事实流（atomic commit）
    └── meta.sqlite                   # SQLite index（best-effort）
```

与 session 目录的区别：
- **Session 目录**（`~/.local/share/smallhead/sessions/`）：默认 storage，append-only JSONL。
- **Inbox 目录**（`~/.local/share/smallhead/inbox/`）：仅 `--durable` 启用时存在。

---

## 6. 与 Agent Loop 集成

```rust
// smallhead-core/src/agent_loop.rs 中的 durable 集成

impl AgentLoop {
    pub async fn run_with_durable(mut self, durable: Arc<DurableSession>) -> Result<()> {
        // 1. resume 拾取
        let resumed = durable.resume().await?;
        for entry in &resumed.interrupted_entries {
            // 发出 interrupted notification 到 UI
            self.commit_message(AgentMessage::Notification {
                kind: "interrupted_on_restart".into(),
                payload: json!({ "entry_id": entry.id }),
            }).await?;
        }
        for input in resumed.pending_inputs {
            self.process_session_input(input).await?;
        }

        // 2. 正常 agent loop —— 每个 commit 都走 durable.commit_atomic()
        loop {
            self.run_one_turn().await?;

            // 3. 检查 inbox
            while let Some(input) = durable.try_recv_input().await? {
                self.process_session_input(input).await?;
            }

            // 4. OUTER 处理 follow-up
            let follow_ups = self.drain_follow_ups().await;
            if follow_ups.is_empty() { break; }
            for msg in follow_ups {
                self.commit_message(msg).await?;
            }
        }
        Ok(())
    }

    async fn commit_message(&self, msg: AgentMessage) -> Result<()> {
        let entry = Entry {
            id: Uuid::now_v7().to_string(),
            parent_id: self.state.last_entry_id.clone(),
            timestamp: now_ms(),
            kind: EntryKind::Message(msg),
        };
        self.durable.commit_atomic(vec![entry]).await?;
        Ok(())
    }
}
```

**关键不变量**：
- 每个 `commit_message` 都走 `commit_atomic`——不绕过 inbox。

---

## 7. 8 个硬不变量（来自 pi-durable spec.md）

| # | 不变量 | 实现 |
|---|--------|------|
| 1 | **所有可见进展都是 durable 的** | 每个 commit 走 `commit_atomic` |
| 2 | **没有 volatile publication path** | 没有"内存里但没落盘"的真理路径 |
| 3 | **状态要么完全提交，要么完全没发生** | tmp → fsync → rename |
| 4 | **重启 = 重放 inbox + 接着上次的 entry** | `resume()` 读取 inbox + history |
| 5 | **inbox 独立于 history** | 物理分离的文件 |
| 6 | **重放 = idempotent** | `prompt_id` 去重 |
| 7 | **interrupt 标记不丢** | `commit_atomic` 立刻 append `Error { kind: "interrupted" }` |
| 8 | **atomic_commit 失败 = 整组不提交** | 单条 entry 序列化失败 → 整组 rejected |

### 7.1 实现每个不变量的代码位置

```rust
// 不变量 1：所有可见进展都是 durable 的
async fn commit_message(&self, msg: AgentMessage) -> Result<()> {
    let entry = Entry::new(msg);
    self.durable.commit_atomic(vec![entry]).await?;  // ← 强制走 atomic
    Ok(())
}

// 不变量 2：没有 volatile publication path
//   - 没有静态消息、没有"memory only" 缓存作为真理
//   - 真理来源永远是 history.jsonl.inbox.jsonl

// 不变量 3：状态要么完全提交，要么完全没发生
//   - commit_atomic 的 tmp → fsync → rename 协议

// 不变量 4：重启 = 重放 inbox + 接着上次的 entry
pub async fn resume(&self) -> Result<ResumedSession> {
    let entries = self.read_all_entries().await?;
    let pending_inputs = self.read_inbox().await?;
    // ...
}

// 不变量 5：inbox 独立于 history
//   - PathBuf self.inbox 与 self.history 物理分离

// 不变量 6：重放 = idempotent
fn dedup_by_prompt_id(inputs: Vec<SessionInput>) -> Vec<SessionInput> {
    let mut seen = HashSet::new();
    inputs.into_iter()
        .filter(|input| {
            let id = match input {
                SessionInput::Prompt { id, .. } => id,
                SessionInput::SwitchModel { .. } => return true,
                SessionInput::Cancel => return true,
                SessionInput::Resume => return true,
            };
            seen.insert(id)
        })
        .collect()
}

// 不变量 7：interrupt 标记不丢
pub async fn resume(&self) -> Result<ResumedSession> {
    let last_running = entries.iter().rev().find(|e| matches!(
        e.kind,
        EntryKind::ProviderTurn { .. } | EntryKind::ToolCall { .. }
    ));
    if let Some(entry) = last_running {
        let interrupted = Entry {
            kind: EntryKind::Error { kind: "interrupted".into(), ... },
            ...
        };
        self.commit_atomic(vec![interrupted]).await?;  // ← 立刻 append
    }
    // ...
}

// 不变量 8：atomic_commit 失败 = 整组不提交
pub async fn commit_atomic(&self, entries: Vec<Entry>) -> Result<...> {
    if entries.is_empty() { return Err(CommitError::Empty); }
    for entry in &entries {
        serde_json::to_vec(entry)?;  // ← 单条失败 = 整组 reject
    }
    // 写 tmp → rename
    ...
}
```

---

## 8. 不变量列表

1. **默认不启用**——`smallhead --durable` 或 config 显式开启。
2. **每个 commit 走 `commit_atomic`**——agent loop 不绕过。
3. **Inbox 独立于 history**——物理分离。
4. **每条 inbox entry 立即 fsync**——inbox 是 admission guarantee。
5. **History 批量 fsync**——5 条 entry 批量（参考 specs/04 §4）。
6. **`commit_atomic` 单条失败 = 整组 reject**——原子性。
7. **重启自动 `resume()`**——标记 running 为 interrupted。
8. **`prompt_id` dedup**——重放幂等。
9. **SQLite 是 best-effort**——崩溃后启动时从 JSONL 重建。
10. **durable entry schema 兼容普通 session entry**——同一 `Entry` enum，可统一查询。

---

## 9. 验收测试

### 9.1 进程 kill -9 → 启动 → resume

```rust
#[tokio::test]
async fn process_kill_resume_recovers_state() {
    let tempdir = tempdir().unwrap();
    let session_id = "test-session".to_string();
    let durable = DurableSession::open(&tempdir.path(), &session_id).await.unwrap();

    // 1. 提交一些 entries
    let entries = vec![
        Entry { id: Uuid::now_v7().to_string(), parent_id: None, timestamp: now_ms(),
                kind: EntryKind::Prompt { content: test_msg(), delivery: Delivery::Steer, resume: false } },
        Entry { id: Uuid::now_v7().to_string(), parent_id: Some(...), timestamp: now_ms(),
                kind: EntryKind::Message(test_user_msg()) },
        Entry { id: Uuid::now_v7().to_string(), parent_id: Some(...), timestamp: now_ms(),
                kind: EntryKind::ProviderTurn { ... } },  // running 状态
    ];
    durable.commit_atomic(entries.clone()).await.unwrap();

    // 2. 模拟 kill -9：不调用任何 cleanup
    drop(durable);

    // 3. 重新启动：resume
    let durable2 = DurableSession::open(&tempdir.path(), &session_id).await.unwrap();
    let resumed = durable2.resume().await.unwrap();

    // 4. 验证
    assert_eq!(resumed.entries.len(), 3);
    assert_eq!(resumed.interrupted_entries.len(), 1);
    assert!(matches!(resumed.interrupted_entries[0].kind, EntryKind::Error { .. }));
    // 最后一个 entry 应该是 interrupted marker
    assert_eq!(resumed.entries.last().unwrap().id, resumed.interrupted_entries[0].id);
}
```

### 9.2 原子性（写一半时 kill）

```rust
#[tokio::test]
async fn atomic_commit_no_corrupt_state_on_partial_write() {
    let tempdir = tempdir().unwrap();
    let session_id = "atomic-test".to_string();
    let durable = DurableSession::open(&tempdir.path(), &session_id).await.unwrap();

    // 1. 先 commit 一组
    durable.commit_atomic(vec![test_entry("a"), test_entry("b")]).await.unwrap();

    // 2. 模拟 commit 写一半 kill：手动留 tmp 文件
    let tmp_path = tempdir.path().join("history.jsonl.tmp");
    tokio::fs::write(&tmp_path, b"{\"id\":\"partial").await.unwrap();

    // 3. resume 应该看到 2 条（a, b），不是 partial
    let durable2 = DurableSession::open(&tempdir.path(), &session_id).await.unwrap();
    let resumed = durable2.resume().await.unwrap();
    assert_eq!(resumed.entries.len(), 2);
    assert!(!tmp_path.exists(), "tmp 文件不应被自动 rename");
}
```

### 9.3 Prompt ID 重放去重

```rust
#[tokio::test]
async fn prompt_id_dedup_on_resume() {
    let tempdir = tempdir().unwrap();
    let session_id = "dedup-test".to_string();
    let durable = DurableSession::open(&tempdir.path(), &session_id).await.unwrap();

    // 1. 写两个相同 prompt_id 的 inbox entries
    durable.write_inbox(SessionInput::Prompt {
        id: "prompt-1".into(),
        content: test_msg(),
        delivery: Delivery::Steer,
        resume: false,
    }).await.unwrap();
    durable.write_inbox(SessionInput::Prompt {
        id: "prompt-1".into(),  // 相同 id
        content: test_msg(),
        delivery: Delivery::Steer,
        resume: false,
    }).await.unwrap();

    // 2. resume
    let resumed = durable.resume().await.unwrap();

    // 3. 验证：只保留一个 prompt-1
    let prompts: Vec<_> = resumed.pending_inputs.iter()
        .filter_map(|i| match i {
            SessionInput::Prompt { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0], "prompt-1");
}
```

### 9.4 不变量 1: 所有可见进展都是 durable 的

```rust
#[tokio::test]
async fn no_volatile_publication_path() {
    // Agent loop 在每个状态变更后必须调 durable.commit_atomic
    // 这个测试：跑一个 turn，验证 history.jsonl 包含所有中间状态
    let tempdir = tempdir().unwrap();
    let session_id = "no-volatile".to_string();
    let durable = Arc::new(DurableSession::open(&tempdir.path(), &session_id).await.unwrap());
    let mock = MockProvider::new(vec![/* ... 完整 turn ... */]);
    let mut loop = AgentLoop::new_with_durable(durable.clone(), Arc::new(mock));
    loop.run().await.unwrap();

    let entries = durable.read_all_entries().await.unwrap();
    // 验证每个 turn 状态都有对应 entry
    assert!(entries.iter().any(|e| matches!(e.kind, EntryKind::Message(_))));
    assert!(entries.iter().any(|e| matches!(e.kind, EntryKind::ToolCall { .. })));
    assert!(entries.iter().any(|e| matches!(e.kind, EntryKind::ToolResult { .. })));
}
```

### 9.5 durable 在没启用时不投

```rust
#[tokio::test]
async fn durable_layer_disabled_by_default() {
    // GIVEN: smallhead.toml 没设 durable.enabled
    let config = Config::default();
    assert!(!config.durable.enabled);

    // WHEN: 启动
    let opts = AgentLoopOptions::from_config(&config);
    assert!(!opts.durable_enabled());
}
```

---

## 10. 与 big-head 对照

| 维度 | big-head v3 checkpoint | smallhead durable |
|------|----------------------|-------------------|
| 模式 | 强制（v3 是主要持久化方式） | opt-in（默认关） |
| Inbox 概念 | 无（v3 没拆 inbox / history） | 拆（opencode V2 借鉴） |
| Commit 协议 | v3 binary format | atomic JSONL（tmp → fsync → rename） |
| Resume | 部分支持 | 完整：interrupted 标记 + 重放 |
| 8 不变量 | 部分（big-head 没显式不变量文档） | 8 个硬不变量（来自 pi-durable） |

big-head v3 已经在 ECS + Action Log 上做了类似工作，但 **inbox 与 history 分离** 是 smallhead 的新东西。

---

## 11. 引用

- 上游 spec:
  - `specs/01-message-model.md`（`AgentMessage` 持久化）
  - `specs/04-session-storage.md`（`Entry` / `EntryKind` schema、JSONL 写入）
- 引用本 spec:
  - `specs/02-agent-loop.md` — `commit_message()` 走 durable
- 源码: `ARCHITECTURE.md` §8
- 方法论: `AGENT_METHODOLOGY.md` §1.6
- 上游 spec:
  - opencode V2 `specs/v2/session.md`
  - pi-durable `packages/durable/docs/spec.md`

---

## 12. 变更日志

- **2026-10-04 v0.1** — 初版。Default off + inbox/history 分离 + atomic commit + 8 个硬不变量 + resume 协议。
- **未来**: 考虑为 inbox 引入 CRDT（pi-durable 风格）支持多端同步。
- **未来**: 考虑支持 `--durable` 与 `--share-world` 组合（团队共享 durable session）。