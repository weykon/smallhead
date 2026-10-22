//! Monad 注册表 —— 主循环可见的目录 + 实例化入口。
//!
//! 对应 `docs/monad-orchestration-pattern.md` §2.2。
//!
//! [`MonadRegistry::instantiate`] 返回 `Box<dyn MonadLoop>`，每次创建独立实例。
//! [`CatalogEntry`] 提供摘要、可见工具与关键词；注册表单独保存 [`MonadId`]。
//! 本模块负责目录和工厂，不执行异步任务，也不管理任务生命周期。

use crate::monad::fixtures::{CalculationMonad, EchoMonad, GreetingMonad};
use crate::monad::loop_trait::{CatalogEntry, MonadLoop};
use crate::monad::protocol::MonadId;
use std::collections::HashSet;
use std::fmt;

type MonadFactory = Box<dyn Fn() -> Box<dyn MonadLoop> + Send + Sync>;
type RegistryEntry = (MonadId, CatalogEntry, MonadFactory);

/// 注册表操作可能产生的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// 注册表里没有这个 monad_id。
    UnknownMonad(MonadId),
    /// 注册表里已经存在同名 monad(重复注册)。
    DuplicateRegistration(MonadId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMonad(id) => write!(f, "unknown monad in registry: {}", id.0),
            Self::DuplicateRegistration(id) => {
                write!(f, "monad already registered: {}", id.0)
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// 方便把错误塞进 `String` 风格的 API。
impl From<RegistryError> for String {
    fn from(e: RegistryError) -> Self {
        e.to_string()
    }
}

/// Monad 注册表 —— 主循环可见的目录 + 实例化入口。
///
/// 见 `docs/monad-orchestration-pattern.md` §2.2。
///
/// ## Send + Sync
///
/// 注册表会被 Scheduler 在自己的 task 里持有、并可能被多个 task 引用,
/// 因此要求 `Send + Sync`。`instantiate()` 返回的 `Box<dyn MonadLoop>`
/// 天然 `Send + Sync`(`MonadLoop: Send + Sync`)。
pub trait MonadRegistry: Send + Sync {
    /// 主循环可见的目录。仅返回短摘要,不暴露实例化路径。
    fn list(&self) -> Vec<CatalogEntry>;

    /// 按 monad_id 取一个 **新实例**。
    ///
    /// Scheduler 在每次派发时调用,避免多个任务复用同一实例
    /// (实例内部有可变状态,如 message history)。
    ///
    /// 返回 `Box<dyn MonadLoop>` —— Slice 4 已定义好,这里直接用。
    fn instantiate(&self, id: &MonadId) -> Result<Box<dyn MonadLoop>, RegistryError>;

    /// 按关键词子串过滤(大小写不敏感)。
    ///
    /// 命中规则:query 与 monad_id / summary / 任一 visible_tool / 任一 keyword 子串匹配
    /// (都转小写后比较)。`limit` 限制返回条数;`0` 表示不限。
    fn search(&self, query: &str, limit: usize) -> Vec<CatalogEntry>;
}

/// 静态注册表。`Vec<(MonadId, CatalogEntry, factory)>` 形式,启动后只读。
///
/// 这是 MVP 第一片;后续 slice 可能加 `RwLock<HashMap<MonadId, ...>>`、
/// 持久化等变体。Slice 2 范围内只保留这个最朴素的版本。
pub struct StaticMonadRegistry {
    entries: Vec<RegistryEntry>,
}

impl fmt::Debug for StaticMonadRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticMonadRegistry")
            .field(
                "entries",
                &self
                    .entries
                    .iter()
                    .map(|(id, e, _)| (id.clone(), e.clone()))
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl StaticMonadRegistry {
    /// 从一组 fixture monad 构造注册表。
    ///
    /// fixture 格式:`(MonadId, CatalogEntry, factory)`。factory 负责造一个新实例。
    ///
    /// # Panics
    ///
    /// 检测到 `monad_id` 重复时 panic —— 这属于编程错误,运行时注册语义
    /// 留给后续 slice(动态注册 + `RegistryError::DuplicateRegistration`)。
    pub fn new(entries: Vec<RegistryEntry>) -> Self {
        // 启动期一次性去重检查 —— 静态注册表不该有重复。
        let mut seen = HashSet::new();
        for (id, _, _) in &entries {
            if !seen.insert(id.clone()) {
                panic!("StaticMonadRegistry: duplicate monad_id {:?}", id);
            }
        }
        Self { entries }
    }

    /// 构造一个内置了 3 个 fixture monad 的注册表:
    /// `calculation` / `echo` / `greeting`(都用本 slice 的真 fixture impl,**不再**
    /// 用 `MainMonad::empty()` 占位)。
    ///
    /// factory 返回未绑定任务的实例，不生成假 task_id 或默认回填槽位。
    /// 调用方需在拿到实例后调 `start(handoff)` 注入真实数据。
    pub fn with_fixtures() -> Self {
        let entries: Vec<RegistryEntry> = vec![
            (
                MonadId("calculation".into()),
                CatalogEntry {
                    summary: "解析并求值 inputs.expression 算术表达式,不接 LLM".into(),
                    visible_tools: vec![],
                    keywords: vec![
                        "calculation".into(),
                        "计算".into(),
                        "arithmetic".into(),
                        "算术".into(),
                    ],
                },
                Box::new(|| Box::new(CalculationMonad::default())),
            ),
            (
                MonadId("echo".into()),
                CatalogEntry {
                    summary: "原样回显 inputs.echo_text 字段,不接 LLM".into(),
                    visible_tools: vec![],
                    keywords: vec!["echo".into(), "回显".into(), "联调".into()],
                },
                Box::new(|| Box::new(EchoMonad::default())),
            ),
            (
                MonadId("greeting".into()),
                CatalogEntry {
                    summary: "用 inputs.name(默认 world)生成问候语,不接 LLM".into(),
                    visible_tools: vec![],
                    keywords: vec![
                        "greeting".into(),
                        "问候".into(),
                        "hello".into(),
                        "打招呼".into(),
                    ],
                },
                Box::new(|| Box::new(GreetingMonad::default())),
            ),
        ];
        Self::new(entries)
    }
}

impl MonadRegistry for StaticMonadRegistry {
    fn list(&self) -> Vec<CatalogEntry> {
        self.entries.iter().map(|(_, e, _)| e.clone()).collect()
    }

    fn instantiate(&self, id: &MonadId) -> Result<Box<dyn MonadLoop>, RegistryError> {
        for (entry_id, _, factory) in &self.entries {
            if entry_id == id {
                return Ok(factory());
            }
        }
        Err(RegistryError::UnknownMonad(id.clone()))
    }

    fn search(&self, query: &str, limit: usize) -> Vec<CatalogEntry> {
        let needle = query.to_lowercase();
        let mut out: Vec<CatalogEntry> = self
            .entries
            .iter()
            .filter(|(id, e, _)| {
                // 命中规则:与 monad_id / summary / 任一 visible_tool / 任一 keyword 子串匹配
                id.0.to_lowercase().contains(&needle)
                    || e.summary.to_lowercase().contains(&needle)
                    || e.visible_tools
                        .iter()
                        .any(|t| t.to_lowercase().contains(&needle))
                    || e.keywords
                        .iter()
                        .any(|k| k.to_lowercase().contains(&needle))
            })
            .map(|(_, e, _)| e.clone())
            .collect();
        if limit > 0 && out.len() > limit {
            out.truncate(limit);
        }
        out
    }
}
