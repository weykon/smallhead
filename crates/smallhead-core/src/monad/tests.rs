//! 协议、注册表与本地业务实例的行为测试。

use super::test_support::{assert_completion_binding, lifecycle_handoff};
use super::*;
use serde_json::json;

/// 最小 handoff JSON 必须能 roundtrip。
#[test]
fn handoff_minimal_roundtrip() {
    let h = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: Some(TaskId::new()),
        monad_id: MonadId("calculation".into()),
        attempt_id: 1,
        objective: "计算 100 加 100".into(),
        inputs: serde_json::Map::from_iter([("expression".into(), json!("100+100"))]),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "返回数值结果".into(),
        result_slot: ResultSlot("calculations.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 1700000000000,
        trace: TraceMeta {
            frame_ref: "frame-rev-001".into(),
            request_id: "req-001".into(),
        },
    };
    let s = serde_json::to_string(&h).unwrap();
    let back: MonadHandoff = serde_json::from_str(&s).unwrap();
    assert_eq!(h, back);
    // 字段名是 snake_case
    assert!(s.contains("schema_version"));
    assert!(s.contains("parent_task_id"));
    assert!(s.contains("result_slot"));
    assert!(s.contains("completion_criteria"));
}

/// parent_task_id=None 时不出现该字段(节省带宽)。
#[test]
fn handoff_root_skips_none_parent() {
    let h = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: None,
        monad_id: MonadId("calculation".into()),
        attempt_id: 1,
        objective: "顶层".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "x".into(),
        result_slot: ResultSlot("r".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    };
    let s = serde_json::to_string(&h).unwrap();
    assert!(!s.contains("parent_task_id"));
    // roundtrip 仍 OK
    let back: MonadHandoff = serde_json::from_str(&s).unwrap();
    assert!(back.parent_task_id.is_none());
}

/// MonadResult::Completed roundtrip + status tag 正确。
#[test]
fn result_completed_roundtrip() {
    let r = MonadResult::Completed {
        task_id: TaskId::new(),
        parent_task_id: Some(TaskId::new()),
        result_slot: ResultSlot("calculations.first".into()),
        monad_id: MonadId("calculation".into()),
        output: json!({"value": 200, "expression": "100+100"}),
        evidence_refs: vec!["exec-1".into()],
        usage: ResourceUsage {
            steps_taken: 2,
            tokens_used: 150,
            elapsed_ms: 1234,
        },
        finished_at_ms: 1700000005000,
    };
    let s = serde_json::to_string(&r).unwrap();
    // tag 是 status,值是 snake_case "completed"
    assert!(s.contains("\"status\":\"completed\""));
    let back: MonadResult = serde_json::from_str(&s).unwrap();
    assert_eq!(r, back);
    assert_eq!(back.task_id(), &r.task_id().clone());
}

/// MonadResult 三种非 completed 变体 + 三元组定位。
#[test]
fn result_other_variants_roundtrip() {
    let task_id = TaskId::new();

    let waiting = MonadResult::WaitingContext {
        task_id: task_id.clone(),
        reason: "缺 expression 字段".into(),
        missing: vec!["expression".into()],
    };
    let s = serde_json::to_string(&waiting).unwrap();
    assert!(s.contains("\"status\":\"waiting_context\""));
    let back: MonadResult = serde_json::from_str(&s).unwrap();
    assert_eq!(waiting, back);

    let failed = MonadResult::Failed {
        task_id: task_id.clone(),
        error: "expression parse error".into(),
        retryable: true,
    };
    let s = serde_json::to_string(&failed).unwrap();
    assert!(s.contains("\"status\":\"failed\""));
    assert!(s.contains("\"retryable\":true"));
    let back: MonadResult = serde_json::from_str(&s).unwrap();
    assert_eq!(failed, back);

    let cancelled = MonadResult::Cancelled {
        task_id,
        reason: "parent cancelled".into(),
    };
    let s = serde_json::to_string(&cancelled).unwrap();
    assert!(s.contains("\"status\":\"cancelled\""));
    let back: MonadResult = serde_json::from_str(&s).unwrap();
    assert_eq!(cancelled, back);
}

/// ContextRef 三种变体的 tag 编码。
#[test]
fn context_ref_tag_encoding() {
    let user_quote = ContextRef::UserQuote {
        text: "先算 100 加 100".into(),
    };
    let s = serde_json::to_string(&user_quote).unwrap();
    assert!(s.contains("\"kind\":\"user_quote\""));

    let prev = ContextRef::PrevResult {
        task_id: TaskId::new(),
        summary: "上一步算出来是 100".into(),
    };
    let s = serde_json::to_string(&prev).unwrap();
    assert!(s.contains("\"kind\":\"prev_result\""));

    let memory = ContextRef::MemoryHit {
        source: "personal_rag".into(),
        ref_id: "doc-42".into(),
        summary: "本月预算相关文档".into(),
    };
    let s = serde_json::to_string(&memory).unwrap();
    assert!(s.contains("\"kind\":\"memory_hit\""));

    // roundtrip
    let back: ContextRef = serde_json::from_str(&s).unwrap();
    assert_eq!(memory, back);
}

/// Budget 的默认字段能从 minimal JSON 还原。
#[test]
fn budget_defaults_fill_in() {
    let j = json!({
        "max_steps": 6,
        "timeout_ms": 30000
    });
    let b: Budget = serde_json::from_value(j).unwrap();
    assert_eq!(b.max_tokens, 4000); // 默认
    assert_eq!(b.concurrent_io, 1); // 默认
}

/// 三种 ID newtype 不会互相误用 —— 编译期阻止,这里只测 serde 表现。
#[test]
fn ids_distinct_types() {
    let task: TaskId = TaskId::new();
    let monad: MonadId = MonadId("calculation".into());
    let slot: ResultSlot = ResultSlot("calculations.first".into());

    // 每个 ID 都序列化为字符串
    assert!(serde_json::to_string(&task).unwrap().starts_with('"'));
    assert!(serde_json::to_string(&monad)
        .unwrap()
        .contains("calculation"));
    assert!(serde_json::to_string(&slot)
        .unwrap()
        .contains("calculations.first"));

    // task_id() helper 工作
    let r = MonadResult::Failed {
        task_id: task.clone(),
        error: "x".into(),
        retryable: false,
    };
    assert_eq!(r.task_id(), &task);
    // 防止 unused 警告
    let _ = monad;
    let _ = slot;
}

/// 完整 handoff + 完整 result 的 wire-format 兼容性快照。
/// 如果字段名/字段顺序变了,这个 test 会失败提醒。
#[test]
fn wire_format_snapshot() {
    let h = MonadHandoff {
        schema_version: SCHEMA_VERSION_V1.to_string(),
        task_id: TaskId::new(),
        parent_task_id: None,
        monad_id: MonadId("calculation".into()),
        attempt_id: 1,
        objective: "算 100+100".into(),
        inputs: serde_json::Map::new(),
        relevant_context: vec![],
        constraints: vec![],
        completion_criteria: "返回数值".into(),
        result_slot: ResultSlot("calculations.first".into()),
        depends_on: vec![],
        budget: Budget::default(),
        issued_at_ms: 0,
        trace: TraceMeta {
            frame_ref: String::new(),
            request_id: String::new(),
        },
    };
    let s = serde_json::to_string(&h).unwrap();
    // 字段顺序固定(serde 默认按 struct 声明顺序)
    let expected_order = vec![
        "schema_version",
        "task_id",
        "monad_id",
        "attempt_id",
        "objective",
        "inputs",
        "completion_criteria",
        "result_slot",
        "budget",
        "issued_at_ms",
        "trace",
    ];
    let mut last_pos = 0;
    for field in expected_order {
        let pos = s
            .find(&format!("\"{}\":", field))
            .unwrap_or_else(|| panic!("missing field {} in {}", field, s));
        assert!(pos >= last_pos, "field {} out of order", field);
        last_pos = pos;
    }
}

/// MonadError 字符串 newtype 行为。
#[test]
fn monad_error_newtype_basics() {
    let e = MonadError::new("boom");
    assert_eq!(e.0, "boom");
    // Display 转发
    assert_eq!(format!("{}", e), "boom");
    // is_err 状态可被 ?
    let r: Result<(), MonadError> = Err(e);
    assert!(r.is_err());
}

// ========================================================================
// Slice 2 测试 —— MonadRegistry + StaticMonadRegistry
// ========================================================================

/// `CatalogEntry` 构造 + 字段可见(直接用 Slice 4 已有的 struct 字段)。
#[test]
fn catalog_entry_basic() {
    let entry = CatalogEntry {
        summary: "顶层对话循环,接收用户原话".into(),
        visible_tools: vec!["echo".into(), "add".into()],
        keywords: vec!["对话".into(), "顶层".into()],
    };
    assert_eq!(entry.summary, "顶层对话循环,接收用户原话");
    assert_eq!(
        entry.visible_tools,
        vec!["echo".to_string(), "add".to_string()]
    );
}

/// `StaticMonadRegistry::with_fixtures()` 列出全部 3 个 fixture。
///
/// Fixture monad 是自包含的(不依赖外部工具),所以 `visible_tools` 故意为空;
/// 路由靠 `monad_id` / `summary` / `keywords` 命中。
#[test]
fn static_registry_list() {
    let reg = StaticMonadRegistry::with_fixtures();
    let entries = reg.list();
    assert_eq!(entries.len(), 3, "with_fixtures 应放 3 个 fixture");
    // 顺序按插入顺序
    assert!(entries[0].summary.contains("算术"));
    assert!(entries[1].summary.contains("回显"));
    assert!(entries[2].summary.contains("问候"));
    // 所有 fixture 都自包含:visible_tools 为空
    for (i, e) in entries.iter().enumerate() {
        assert!(
            e.visible_tools.is_empty(),
            "fixture[{i}] 的 visible_tools 应为空(自包含 monad 不需要工具),got={:?}",
            e.visible_tools
        );
        assert!(
            !e.keywords.is_empty(),
            "fixture[{i}] 必须配置 keywords 用于路由"
        );
    }
    // debug 不爆炸
    let _ = format!("{:?}", reg);
}

/// `search("cal", 10)` 命中 calculation。
#[test]
fn static_registry_search() {
    let reg = StaticMonadRegistry::with_fixtures();

    // 用 "cal" 应该命中 calculation(monad_id 子串)
    let hits = reg.search("cal", 10);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("算术"));

    // 大小写不敏感
    let hits = reg.search("CAL", 10);
    assert_eq!(hits.len(), 1);

    // 按 keyword 命中("echo" 出现在 echo fixture 的 keywords 里)
    let hits = reg.search("echo", 10);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("回显"));
    assert!(
        hits[0].keywords.iter().any(|k| k == "echo"),
        "echo fixture 的 keywords 应包含 \"echo\""
    );

    // 按中文 keyword 命中
    let hits = reg.search("打招呼", 10);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("问候"));

    // 没命中
    let hits = reg.search("nothing-here-zzz", 10);
    assert!(hits.is_empty());

    // limit 生效
    let hits = reg.search("", 2); // 空 query → 全命中 → 取前 2
    assert_eq!(hits.len(), 2);
}

/// `instantiate` 未知 monad 返回 `RegistryError::UnknownMonad`。
#[test]
fn static_registry_unknown_returns_err() {
    let reg = StaticMonadRegistry::with_fixtures();
    let bogus = MonadId("nonexistent-monad".into());
    let err = match reg.instantiate(&bogus) {
        Err(e) => e,
        Ok(_) => panic!("expected error for unknown monad"),
    };
    match &err {
        RegistryError::UnknownMonad(id) => assert_eq!(id, &bogus),
        other => panic!("expected UnknownMonad, got {other:?}"),
    }
    // Display 输出含 monad_id
    let s = format!("{err}");
    assert!(s.contains("nonexistent-monad"), "Display 应含 id: {s}");

    // 已知 id 返回非 None 实例(MVP fixture 用 MainMonad::empty() 包装,
    // 所以 monad_id 是全局 "main" —— 这是 MVP 占位的已知妥协,
    // 等真有非顶层 monad 后,工厂应返回真正属于该 id 的实例)
    let inst = reg.instantiate(&MonadId("calculation".into())).unwrap();
    let _ = inst.monad_id(); // 验证能调用
}

/// `RegistryError` 满足 `std::error::Error` 契约 + 能转 String。
#[test]
fn registry_error_traits() {
    let err = RegistryError::DuplicateRegistration(MonadId("dup".into()));
    // Display
    let s = format!("{err}");
    assert!(s.contains("dup"));
    // std::error::Error trait object
    let _: &dyn std::error::Error = &err;
    // Into<String>
    let s2: String = err.into();
    assert!(s2.contains("dup"));
}

/// 静态注册表构造时重复 monad_id 必须 panic。
#[test]
#[should_panic(expected = "duplicate monad_id")]
fn static_registry_duplicate_panics() {
    type TestEntry = (
        MonadId,
        CatalogEntry,
        Box<dyn Fn() -> Box<dyn MonadLoop> + Send + Sync>,
    );
    let entries: Vec<TestEntry> = vec![
        (
            MonadId("dup".into()),
            CatalogEntry {
                summary: "a".into(),
                visible_tools: vec![],
                keywords: vec![],
            },
            Box::new(|| Box::new(MainMonad::empty())),
        ),
        (
            MonadId("dup".into()),
            CatalogEntry {
                summary: "b".into(),
                visible_tools: vec![],
                keywords: vec![],
            },
            Box::new(|| Box::new(MainMonad::empty())),
        ),
    ];
    let _ = StaticMonadRegistry::new(entries);
}
#[tokio::test]
async fn fixtures_preserve_binding_and_reject_invalid_lifecycle() {
    let registry = StaticMonadRegistry::with_fixtures();
    for id in ["echo", "calculation", "greeting"] {
        let mut instance = registry.instantiate(&MonadId::new(id)).unwrap();
        assert!(
            instance.step().await.is_err(),
            "factory must not invent a task"
        );
        let first = lifecycle_handoff(id);
        instance.start(first.clone()).await.unwrap();
        assert!(instance.start(lifecycle_handoff(id)).await.is_err());
        assert_completion_binding(instance.step().await.unwrap(), &first);
        assert!(instance.step().await.is_err(), "no duplicate completion");
        let mut second = lifecycle_handoff(id);
        second.parent_task_id = None;
        second.result_slot = ResultSlot("second".into());
        instance.start(second.clone()).await.unwrap();
        assert_completion_binding(instance.step().await.unwrap(), &second);
        instance.start(lifecycle_handoff(id)).await.unwrap();
        instance.cancel().await.unwrap();
        assert!(
            instance.step().await.is_err(),
            "cancelled task must not complete"
        );
    }
}

#[tokio::test]
async fn rejected_handoffs_leave_instances_unbound() {
    let registry = StaticMonadRegistry::with_fixtures();
    for id in ["main", "echo", "calculation", "greeting"] {
        let mut instance: Box<dyn MonadLoop> = if id == "main" {
            Box::new(MainMonad::empty())
        } else {
            registry.instantiate(&MonadId::new(id)).unwrap()
        };
        for invalid in 0..6 {
            let mut handoff = lifecycle_handoff(id);
            match invalid {
                0 => handoff.monad_id = MonadId::new("wrong"),
                1 => handoff.task_id = TaskId(" ".into()),
                2 => handoff.result_slot = ResultSlot("".into()),
                3 => handoff.parent_task_id = Some(TaskId("".into())),
                4 => handoff.schema_version = "unsupported".into(),
                _ => handoff.attempt_id = 0,
            }
            assert!(instance.start(handoff).await.is_err());
        }
        instance.start(lifecycle_handoff(id)).await.unwrap();
    }
}

#[tokio::test]
async fn factory_instances_do_not_share_task_state() {
    let registry = StaticMonadRegistry::with_fixtures();
    for id in ["echo", "calculation", "greeting"] {
        let mut a = registry.instantiate(&MonadId::new(id)).unwrap();
        let mut b = registry.instantiate(&MonadId::new(id)).unwrap();
        let ha = lifecycle_handoff(id);
        let hb = lifecycle_handoff(id);
        a.start(ha.clone()).await.unwrap();
        b.start(hb.clone()).await.unwrap();
        assert_completion_binding(b.step().await.unwrap(), &hb);
        assert_completion_binding(a.step().await.unwrap(), &ha);
    }
}

#[tokio::test]
async fn calculation_waiting_context_can_be_resubmitted() {
    let mut m = CalculationMonad::default();
    let mut h = lifecycle_handoff("calculation");
    h.inputs.clear();
    m.start(h.clone()).await.unwrap();
    match m.step().await.unwrap() {
        MonadResult::WaitingContext { task_id, .. } => assert_eq!(task_id, h.task_id),
        other => panic!("expected WaitingContext, got {other:?}"),
    }
    assert!(m.step().await.is_err());
    h.inputs
        .insert("expression".into(), serde_json::json!("5*6"));
    m.start(h.clone()).await.unwrap();
    assert_completion_binding(m.step().await.unwrap(), &h);
}

#[tokio::test]
async fn direct_fixture_construction_validates_and_preserves_parent() {
    let echo = lifecycle_handoff("echo");
    assert_completion_binding(
        EchoMonad::from_handoff(&echo)
            .unwrap()
            .step()
            .await
            .unwrap(),
        &echo,
    );
    let calc = lifecycle_handoff("calculation");
    assert_completion_binding(
        CalculationMonad::from_handoff(&calc)
            .unwrap()
            .step()
            .await
            .unwrap(),
        &calc,
    );
    let greeting = lifecycle_handoff("greeting");
    assert_completion_binding(
        GreetingMonad::from_handoff(&greeting)
            .unwrap()
            .step()
            .await
            .unwrap(),
        &greeting,
    );
    assert!(EchoMonad::from_handoff(&calc).is_err());
    assert!(CalculationMonad::from_handoff(&greeting).is_err());
    assert!(GreetingMonad::from_handoff(&echo).is_err());
}
