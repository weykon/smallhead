//! 日志模型：有序事件、过滤和 JSON 详情，不依赖终端渲染。
use smallhead_core::diagnostics::DiagnosticEvent;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub sequence: u64,
    pub task: String,
    pub record: DiagnosticEvent,
}

pub const FILTERS: &[&str] = &[
    "all",
    "request",
    "message",
    "block",
    "tool",
    "plan",
    "monad",
    "lens",
    "route",
    "response",
    "error",
    "process",
    "search_chain",
    "transport",
    "ui",
];

/// 光标距视窗顶/底的最小留白行数(vim `scrolloff`)。0 = 旧行为(光标贴边)。
pub const DEFAULT_SCROLLOFF: usize = 5;

#[derive(Default)]
pub struct Logs {
    pub entries: Vec<LogEntry>,
    pub selected: usize,
    pub follow: bool,
    pub filter: usize,
    pub detail_scroll: u16,
    /// 第一行可见条目在 [`Self::visible`] 切片中的下标。渲染层每帧调
    /// [`Self::ensure_visible`] 调整,光标与该下标联动。
    pub view_offset: usize,
    /// 光标与视窗顶/底的最小留白行数。`0` 表示光标允许贴边(老行为)。
    /// 在跟随模式下,新事件到来时光标停在 `view_h - 1 - scrolloff` 行,
    /// 视窗底部留出 `scrolloff` 行空白作为"未来"提示。
    pub scrolloff: usize,
    next: u64,
    pub evicted: u64,
}

impl Logs {
    pub fn new() -> Self {
        Self {
            follow: true,
            view_offset: 0,
            scrolloff: DEFAULT_SCROLLOFF,
            ..Self::default()
        }
    }
    pub fn push(&mut self, task: &str, record: DiagnosticEvent) -> u64 {
        self.next += 1;
        let sequence = self.next;
        self.entries.push(LogEntry {
            sequence,
            task: task.into(),
            record,
        });
        // Keep whole request payloads, but bound retained event count.
        if self.entries.len() > 3000 {
            let removed = self.entries.remove(0);
            self.evicted += 1;
            if self.filter == 0 || removed.record.layer == FILTERS[self.filter] {
                self.selected = self.selected.saturating_sub(1);
                self.view_offset = self.view_offset.saturating_sub(1);
            }
        }
        if self.follow {
            self.selected = self.visible().len().saturating_sub(1);
            self.detail_scroll = 0;
        }
        sequence
    }
    pub fn visible(&self) -> Vec<&LogEntry> {
        self.entries
            .iter()
            .filter(|e| self.filter == 0 || e.record.layer == FILTERS[self.filter])
            .collect()
    }
    /// 当前筛选下可见条目数。比 `self.visible().len()` 省一次堆分配,
    /// 也避免调用方在 `visible()` 借走 `&self` 时无法继续 mut borrow [`Self::ensure_visible`]。
    pub fn visible_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| self.filter == 0 || e.record.layer == FILTERS[self.filter])
            .count()
    }
    pub fn current(&self) -> Option<&LogEntry> {
        self.visible().get(self.selected).copied()
    }
    pub fn move_selection(&mut self, delta: isize) {
        self.follow = false;
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.visible().len().saturating_sub(1));
        self.detail_scroll = 0;
    }
    /// 设置 scrolloff。`0` 即老行为(光标贴边);`>= 1` 时在视窗顶/底各留
    /// 出该行数的"缓冲",避免光标在最新事件时贴底。
    pub fn set_scrolloff(&mut self, n: usize) {
        self.scrolloff = n;
    }
    /// 根据当前 [`Self::selected`] 与可视行数 `view_h` 调整 [`Self::view_offset`],
    /// 使光标在视窗内距顶/底至少 `scrolloff` 行(模仿 vim 的 `scrolloff`)。
    ///
    /// 渲染层每帧调一次,`move_selection` / `push` / 点击选中不必各自管 —— 视窗
    /// 状态是渲染关注的,不是数据状态。
    ///
    /// `view_h` = 列表 panel border 已扣后的可视行数(行高,不是像素)。
    /// 终端过窄时 `view_h` 会很小,`scrolloff` 会被夹到 `(view_h-1)/2` 以保证
    /// 约束 `view_offset + scrolloff ≤ selected ≤ view_offset + view_h - 1 - scrolloff` 有解。
    ///
    /// `total` = 当前筛选下可见条目数(由调用方用 [`Self::visible_count`] 算,
    /// 避免函数内部 `self.visible()` 与 `&mut self` 借用冲突)。
    pub fn ensure_visible(&mut self, view_h: usize, total: usize) {
        if view_h == 0 {
            return;
        }
        if total == 0 {
            self.view_offset = 0;
            self.selected = 0;
            return;
        }
        // scrolloff 不能超过 (view_h-1)/2,否则约束区间 [v+s, v+h-1-s] 无解。
        let scrolloff = self.scrolloff.min(view_h.saturating_sub(1) / 2);
        // 即使零留白，也必须保证选中项落在传给 List 的切片内。
        self.selected = self.selected.min(total - 1);
        let s = self.selected;
        if self.view_offset + scrolloff > s {
            // 光标离顶太近,把视窗向下挪(scrolloff 行缓冲)
            self.view_offset = s.saturating_sub(scrolloff);
        } else if s + scrolloff + 1 > self.view_offset + view_h {
            // 光标离底太近或已被滚出底部,向上推
            self.view_offset = s + scrolloff + 1 - view_h;
        }
        // 上面两条分支已经把 view_offset 拉到 [s - scrolloff, s + scrolloff + 1 - view_h]
        // 区间,光标位置天然满足 [scrolloff, view_h - 1 - scrolloff]。
        // 无需再 clamp —— 老版本会再夹一次 `total - view_h`,导致跟随模式时光标
        // 反而从 14 行被拽回到 19 行(scrolloff 失效)。
    }
    pub fn cycle_filter(&mut self) {
        self.filter = (self.filter + 1) % FILTERS.len();
        self.view_offset = 0;
        self.selected = 0;
        self.detail_scroll = 0;
        self.follow = false;
    }
    pub fn latest_request(&mut self) {
        self.filter = 1;
        self.selected = self.visible().len().saturating_sub(1);
        self.detail_scroll = 0;
        self.follow = false;
    }
    pub fn export(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                serde_json::json!({"sequence":e.sequence,"task":e.task,"record":e.record})
                    .to_string()
                    + "\n"
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filtering_selection_and_order_are_stable() {
        let mut logs = Logs::new();
        for layer in ["request", "tool", "request"] {
            logs.push(
                "task",
                DiagnosticEvent {
                    timestamp_ms: 0,
                    layer: layer.into(),
                    event: "event".into(),
                    data: serde_json::json!({"中文":"完整请求"}),
                },
            );
        }
        logs.latest_request();
        assert_eq!(logs.current().unwrap().sequence, 3);
        logs.move_selection(-1);
        assert_eq!(logs.current().unwrap().sequence, 1);
        assert!(logs.export().contains("完整请求"));
    }
    /// 验证 scrolloff 行为:光标在最末时停在 view_h-1-scrolloff 行,
    /// 视窗底部留 scrolloff 行空白;新事件来时 view_offset 上推,光标位置不动。
    #[test]
    fn ensure_visible_keeps_cursor_within_scrolloff_margin() {
        let mut logs = Logs::new();
        logs.set_scrolloff(5);
        // 灌 100 条,触发 follow 把光标钉到末位
        for i in 0..100 {
            logs.push(
                "t",
                DiagnosticEvent {
                    timestamp_ms: 0,
                    layer: "tool".into(),
                    event: format!("e{i}"),
                    data: serde_json::json!({}),
                },
            );
        }
        assert_eq!(logs.selected, 99);
        // view_h=20,光标停在第 14 行(20-1-5),view_offset=85
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 85, "末位应停在 14 行,留 5 行空白");
        // 又来一条:光标继续停在第 14 行(100-86=14),view_offset 上推到 86
        logs.push(
            "t",
            DiagnosticEvent {
                timestamp_ms: 0,
                layer: "tool".into(),
                event: "new".into(),
                data: serde_json::json!({}),
            },
        );
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 86);
        // 用户跳到中段(50),view_offset 应保证 50 距视窗顶至少 5
        logs.selected = 50;
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(
            logs.view_offset, 45,
            "光标在 50 时 view_offset=45, 距顶 5 行"
        );
        // 用户跳到顶(0),view_offset=0
        logs.selected = 0;
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 0);
        // 用户又跳到末位,确保能回到"光标在中下"位置
        logs.selected = 99;
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 85);
    }
    /// 零留白仍滚动到选中项，切片不依赖 List 自动滚动。
    #[test]
    fn scrolloff_zero_preserves_old_behavior() {
        let mut logs = Logs::new();
        logs.set_scrolloff(0);
        for i in 0..30 {
            logs.push(
                "t",
                DiagnosticEvent {
                    timestamp_ms: 0,
                    layer: "tool".into(),
                    event: format!("e{i}"),
                    data: serde_json::json!({}),
                },
            );
        }
        logs.view_offset = 0;
        logs.selected = 29;
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 10);
        logs.selected = 0;
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 0);
    }
    /// 视窗过窄时 scrolloff 自动夹到 (view_h-1)/2,不会约束无解。
    #[test]
    fn scrolloff_clamped_to_half_view_when_window_tiny() {
        let mut logs = Logs::new();
        logs.set_scrolloff(20); // 用户想要 20 行留白
        for i in 0..10 {
            logs.push(
                "t",
                DiagnosticEvent {
                    timestamp_ms: 0,
                    layer: "tool".into(),
                    event: format!("e{i}"),
                    data: serde_json::json!({}),
                },
            );
        }
        logs.selected = 5;
        logs.ensure_visible(4, logs.visible_count());
        // 光标在 5,view_offset 至少 4 (5-1); 末位约束 5+1+1=7 > v+4 → v≥3
        // 即:5 在 [v+1, v+2] 区间 → v=3 或 4
        assert!(logs.view_offset == 3 || logs.view_offset == 4);
    }
    fn event(layer: &str) -> DiagnosticEvent {
        DiagnosticEvent {
            timestamp_ms: 0,
            layer: layer.into(),
            event: "e".into(),
            data: serde_json::json!({}),
        }
    }
    #[test]
    fn resize_to_one_row_keeps_selected_visible() {
        let mut logs = Logs::new();
        for _ in 0..100 {
            logs.push("t", event("tool"));
        }
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.selected - logs.view_offset, 14);
        logs.ensure_visible(1, logs.visible_count());
        assert_eq!(logs.selected, logs.view_offset);
        logs.ensure_visible(2, logs.visible_count());
        assert!(logs.selected >= logs.view_offset && logs.selected < logs.view_offset + 2);
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.selected - logs.view_offset, 5);
    }
    #[test]
    fn filtered_eviction_preserves_browsed_event_and_view() {
        let mut logs = Logs::new();
        logs.push("t", event("tool"));
        for _ in 1..3000 {
            logs.push("t", event("request"));
        }
        logs.latest_request();
        logs.ensure_visible(20, logs.visible_count());
        let selected = logs.current().unwrap().sequence;
        let offset = logs.view_offset;
        logs.push("t", event("tool")); // 淘汰筛选外的 tool
        assert_eq!(logs.current().unwrap().sequence, selected);
        assert_eq!(logs.view_offset, offset);
        logs.push("t", event("tool")); // 淘汰可见的 request
        assert_eq!(logs.current().unwrap().sequence, selected);
        assert_eq!(logs.view_offset, offset - 1);
    }
    #[test]
    fn follow_keeps_constant_margin_and_filter_resets_view() {
        let mut logs = Logs::new();
        for _ in 0..100 {
            logs.push("t", event("tool"));
        }
        for _ in 0..12 {
            logs.ensure_visible(20, logs.visible_count());
            assert_eq!(logs.selected - logs.view_offset, 14);
            logs.push("t", event("tool"));
        }
        logs.cycle_filter();
        logs.ensure_visible(20, logs.visible_count());
        assert_eq!(logs.view_offset, 0);
        assert_eq!(logs.selected, 0);
    }
}
