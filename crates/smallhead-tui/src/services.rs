//! 「服务」页：ServiceView 列表 + 详情 tab。
//!
//! 与 [`crate::processes`] 不同,这里只看 [`ServiceView`],不直接读
//! `ProcessManager` 的执行列表。所有启停、检查、忘记都走
//! `ServiceManager`,让 UI 与 Agent 共享同一事实源。
//!
//! 右栏三 tab:
//! - 概览:Agent 写入的自由文本 `description` + 技术字段(状态/管理方式/
//!   健康地址/工作目录/启动命令/执行 ID 等),按设计稿 2026-10-15 更新:
//!   **不**显示 use_case / accessible / authenticated 等固定字段。
//! - 输出:读 `App::service_output_cache` 里的快照(由 `tick()` 预拉取),
//!   因为 ratatui 渲染路径是同步的,而 `ProcessManager::read_output` 是 async。
//! - 事件:从 `service-events.jsonl` 读取;DescriptionUpdated 单独高亮。
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use smallhead_core::execution::smallhead_process::service::{
    ServiceEvent, ServiceState, ServiceView,
};

pub const EVENT_CACHE_LIMIT: usize = 512;
const EVENT_READ_BYTES: u64 = 256 * 1024;

pub fn read_recent_events(path: &Path) -> VecDeque<ServiceEvent> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return VecDeque::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(EVENT_READ_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return VecDeque::new();
    }
    let mut bytes = Vec::new();
    if file.take(EVENT_READ_BYTES).read_to_end(&mut bytes).is_err() {
        return VecDeque::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    let mut events: Vec<_> = lines
        .filter_map(|line| serde_json::from_str::<ServiceEvent>(line).ok())
        .rev()
        .take(EVENT_CACHE_LIMIT)
        .collect();
    events.reverse();
    events.into()
}

pub fn view_state_label(view: &ServiceView) -> (&'static str, Color) {
    if view.health_url.is_empty() && view.state == ServiceState::Ready {
        ("运行中", Color::Green)
    } else {
        state_label_and_color(&view.state)
    }
}

use crate::app::{App, ServiceOutputMeta, ServiceOutputSnapshot};

/// 把 `ServiceState` 翻译成 (中文 label, 颜色) 配色。设计稿 §3 要求
/// 状态同时用文字和颜色,避免只靠红色判断错误。
pub fn state_label_and_color(state: &ServiceState) -> (&'static str, Color) {
    match state {
        ServiceState::Starting => ("启动中", Color::Yellow),
        ServiceState::Ready => ("就绪", Color::Green),
        ServiceState::Degraded => ("健康异常", Color::Red),
        ServiceState::Stopped => ("未启动", Color::DarkGray),
        ServiceState::Failed => ("启动失败", Color::Red),
        ServiceState::Unknown => ("未确认", Color::Magenta),
    }
}

/// "管理方式"中文 + 颜色,仅依赖 view 字段。
pub fn management_label_and_color(view: &ServiceView) -> (&'static str, Color) {
    if view.external {
        ("外部", Color::LightBlue)
    } else {
        ("本应用管理", Color::LightCyan)
    }
}

/// 渲染整页服务面板:左列列表,右列详情 tab。
pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    if app.services_mcp {
        crate::mcp_services::draw(f, &mut app.mcp_view, &app.runtime.mcp, area);
        return;
    }
    if let Err(error) = &app.runtime.execution {
        f.render_widget(
            Paragraph::new(format!(
                "服务管理器初始化失败\n\n{error}\n\n服务列表尚未读取，不能据此判断服务登记为空。\n若提示目录被占用，请先退出另一个 Smallhead TUI 或使用同一目录的进程，再重新启动本窗口。"
            ))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" 服务不可用 ")),
            area,
        );
        return;
    }
    if area.width < 100 {
        if app.service_detail_focus {
            draw_detail(f, app, area);
        } else {
            draw_list(f, app, area);
        }
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    draw_list(f, app, columns[0]);
    draw_detail(f, app, columns[1]);
}

/// 左栏:从 `ServiceManager::service_view_all` 拉快照,按列表渲染。
/// 选中由 [`App::processes::service_selected_name`] 持有(沿用进程页服务区的
/// 稳定名字选择,Commit 6 起会搬到本页专用字段)。
fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let views = current_views(app);
    let selected_index = app
        .processes
        .service_selected_name
        .as_ref()
        .and_then(|name| views.iter().position(|v| &v.name == name));
    let mut list_state = ListState::default().with_selected(selected_index);

    let items: Vec<ListItem> = if views.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(尚无服务)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        views
            .iter()
            .enumerate()
            .map(|(idx, view)| {
                ListItem::new(build_row(view, idx == selected_index.unwrap_or(usize::MAX)))
            })
            .collect()
    };
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 服务 · s 启动 · r 检查 · x 停止 · m MCP · ↑↓ 选行 · Enter 详情 "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ "),
        area,
        &mut list_state,
    );
    // 注册点击区:每行一个 SelectService(idx) 命中区,与列表对齐。
    for (row, idx) in (list_state.offset()..views.len()).enumerate() {
        let y = area.y + 1 + row as u16;
        if y >= area.y.saturating_add(area.height).saturating_sub(1) {
            break;
        }
        app.click_map.register(
            area.x + 1,
            y,
            area.width.saturating_sub(2),
            1,
            crate::app::ClickAction::SelectService(idx),
        );
    }
}

/// 渲染单行:name + 状态 + 管理方式 + 期望运行 + (宽屏) 最近检查 / 错误。
fn build_row(view: &ServiceView, is_selected: bool) -> Line<'static> {
    let (state_text, state_color) = view_state_label(view);
    let (mgmt_text, mgmt_color) = management_label_and_color(view);
    let desired_text = if view.desired_running {
        "期望:运行"
    } else {
        "期望:停止"
    };
    let desired_color = if view.desired_running {
        Color::LightGreen
    } else {
        Color::DarkGray
    };
    let last_reason = if let Some(o) = &view.last_observation {
        if o.reason.is_empty() {
            None
        } else {
            Some(o.reason.clone())
        }
    } else if view.detail.is_empty() {
        None
    } else {
        Some(view.detail.clone())
    };
    let name_color = if is_selected {
        Color::Black
    } else {
        Color::White
    };
    let name_bg = if is_selected {
        Color::Cyan
    } else {
        Color::Reset
    };
    let mut spans = vec![
        Span::styled(
            format!("{:<14}", truncate(&view.name, 14)),
            Style::default().fg(name_color).bg(name_bg),
        ),
        Span::styled(
            format!("{:<8}", state_text),
            Style::default().fg(state_color),
        ),
        Span::styled(
            format!("{:<10}", mgmt_text),
            Style::default().fg(mgmt_color),
        ),
        Span::styled(
            format!("{:<8}", desired_text),
            Style::default().fg(desired_color),
        ),
    ];
    if let Some(reason) = last_reason {
        spans.push(Span::styled(
            format!(" {}", truncate(&reason, 60)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// 右栏 tab 头 + 选中 tab 的内容。
fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    // 上方 1 行 tab 头,下方详情。
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);
    draw_tab_strip(f, app, chunks[0]);
    // 当前选中服务的 ServiceView;没有选或选中的不在列表里 → 提示。
    let view = current_views(app)
        .into_iter()
        .find(|v| Some(&v.name) == app.processes.service_selected_name.as_ref());
    let block = Block::default().borders(Borders::ALL).title(" 详情 ");
    let body_area = chunks[1].inner(Margin::new(1, 1));
    f.render_widget(block, chunks[1]);
    let tab = app.service_detail_tab;
    let paragraph = match tab {
        0 => build_overview_paragraph(
            view.as_ref(),
            view.as_ref().and_then(|view| {
                app.service_event_cache.iter().rev().find(|event| {
                    event.name == view.name
                        && event.kind == smallhead_core::execution::smallhead_process::service::ServiceEventKind::OperationFinished
                        && event.reason.as_deref().is_some_and(|reason| reason != "ok")
                })
            }),
        ),
        1 => build_output_paragraph(app, view.as_ref()),
        _ => build_events_paragraph(app, view.as_ref()),
    };
    f.render_widget(paragraph.scroll((app.service_detail_scroll, 0)), body_area);
}

/// 画 3 个 tab 头,当前 tab 用反色背景。
fn draw_tab_strip(f: &mut Frame, app: &App, area: Rect) {
    let labels = ["[概览]", "[输出]", "[事件]"];
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (idx, label) in labels.iter().enumerate() {
        if idx == app.service_detail_tab {
            spans.push(Span::styled(
                format!(" {label} "),
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!(" {label} "),
                Style::default().fg(Color::White),
            ));
        }
    }
    spans.push(Span::styled(
        "  (Tab 切换)".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 概览 tab:Agent 自由文本 description(若有) + 技术字段。
/// 设计稿 2026-10-15:不展示 use_case / accessible / authenticated 固定字段。
fn build_overview_paragraph(
    view: Option<&ServiceView>,
    recent_failure: Option<&ServiceEvent>,
) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let Some(view) = view else {
        lines.push(Line::from(Span::styled(
            "未选择服务。↑↓ 在左侧列表里选一行。",
            Style::default().fg(Color::DarkGray),
        )));
        return Paragraph::new(lines).wrap(Wrap { trim: false });
    };
    let (state_text, state_color) = view_state_label(view);
    let (mgmt_text, mgmt_color) = management_label_and_color(view);
    // 概览顶部:Agent 写的自由文本说明。空时给降级提示。
    lines.push(Line::from(Span::styled(
        "── 服务说明 ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    if let Some(desc) = view.description.as_ref().filter(|s| !s.is_empty()) {
        for chunk in wrap_text(desc, 80) {
            lines.push(Line::from(Span::styled(
                chunk,
                Style::default().fg(Color::White),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(暂无服务说明。Agent 可调用 service_update_description 写一段自由文本。)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    if let Some(updated) = view.description_updated_at {
        let timestamp: chrono::DateTime<chrono::Local> = updated.into();
        lines.push(Line::from(format!(
            "说明更新: {}",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        )));
    }
    // 技术字段
    lines.push(Line::from(Span::styled(
        "── 技术详情 ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled("  状态: ", Style::default().fg(Color::DarkGray)),
        Span::styled(state_text, Style::default().fg(state_color)),
    ]));
    if let Some(reason) = recent_failure.and_then(|event| event.reason.as_ref()) {
        lines.push(Line::from(Span::styled(
            format!("  最近操作错误: {reason}"),
            Style::default().fg(Color::Red),
        )));
    }
    if view.health_url.is_empty() && view.state == ServiceState::Ready {
        lines.push(Line::from("进程已启动；未配置健康/身份检查"));
    }
    lines.push(Line::from(vec![
        Span::styled("  管理方式: ", Style::default().fg(Color::DarkGray)),
        Span::styled(mgmt_text, Style::default().fg(mgmt_color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  期望: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if view.desired_running {
                "运行"
            } else {
                "停止"
            },
            Style::default().fg(if view.desired_running {
                Color::LightGreen
            } else {
                Color::DarkGray
            }),
        ),
    ]));
    if let Some(op) = view.in_flight_operation {
        lines.push(Line::from(vec![
            Span::styled("  正在操作: ", Style::default().fg(Color::DarkGray)),
            Span::styled(op.0.to_string(), Style::default().fg(Color::Yellow)),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("  健康地址: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            view.health_url.clone(),
            Style::default().fg(Color::LightBlue),
        ),
    ]));
    if let Some(workdir) = &view.workdir {
        lines.push(Line::from(vec![
            Span::styled("  工作目录: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                workdir.display().to_string(),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    if !view.startup_command_display.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  启动命令: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                view.startup_command_display.clone(),
                Style::default().fg(Color::White),
            ),
        ]));
    }
    if let Some(eid) = view.execution_id {
        lines.push(Line::from(vec![
            Span::styled("  执行 ID: ", Style::default().fg(Color::DarkGray)),
            Span::styled(eid.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    if !view.detail.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  最近结果: ", Style::default().fg(Color::DarkGray)),
            Span::styled(view.detail.clone(), Style::default().fg(Color::White)),
        ]));
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// 输出 tab:读 `App::service_output_cache` 里由 tick 预拉取的快照。
fn build_output_paragraph(app: &App, view: Option<&ServiceView>) -> Paragraph<'static> {
    let Some(view) = view else {
        return Paragraph::new(Line::from(Span::styled(
            "未选择服务。",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
    };
    if view.execution_id.is_none() {
        return Paragraph::new(Line::from(Span::styled(
            "未由本应用采集(外部服务或尚未启动)。",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
    }
    let Some(snapshot) = output_snapshot_for(app, &view.name) else {
        return Paragraph::new(Line::from(Span::styled(
            "输出缓存尚未就绪,等下一次 tick。",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
    };
    let meta = app
        .service_output_meta
        .as_ref()
        .filter(|m| m.service_name == view.name)
        .cloned();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("── stdout(末尾 · {} ) ──", meta_label(meta.as_ref())),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    push_snapshot_lines(&mut lines, &snapshot.stdout, "stdout 为空。");
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "── stderr(末尾) ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    push_snapshot_lines(&mut lines, &snapshot.stderr, "stderr 为空。");
    if let Some(err) = &snapshot.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("读输出失败: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// 事件 tab:从 service-events.jsonl 读最近 50 条,时间倒序。
/// DescriptionUpdated 事件额外加 Magenta 高亮。
fn build_events_paragraph(app: &App, view: Option<&ServiceView>) -> Paragraph<'static> {
    let Some(view) = view else {
        return Paragraph::new(Line::from(Span::styled(
            "未选择服务。",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: false });
    };
    let recent: Vec<String> = app
        .service_event_cache
        .iter()
        .rev()
        .filter(|event| event.name == view.name)
        .take(50)
        .filter_map(|event| serde_json::to_string(event).ok())
        .collect();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if recent.is_empty() {
        lines.push(Line::from(Span::styled(
            "该服务尚无事件。",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for raw in recent {
            let highlight = raw.contains("description_updated");
            let color = if highlight {
                Color::Magenta
            } else {
                Color::White
            };
            for chunk in wrap_text(&raw, 80) {
                lines.push(Line::from(Span::styled(chunk, Style::default().fg(color))));
            }
        }
    }
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// 读 OutputPage 后,把 text 拆成行追加到 lines。空结果加占位提示。
fn push_snapshot_lines(lines: &mut Vec<Line<'static>>, text: &str, empty_msg: &str) {
    if text.is_empty() {
        lines.push(Line::from(Span::styled(
            empty_msg.to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for chunk in wrap_text(text, 80) {
            lines.push(Line::from(Span::styled(
                chunk,
                Style::default().fg(Color::White),
            )));
        }
    }
}

/// 从 `App::service_output_cache` 拿当前服务的快照。
fn output_snapshot_for(app: &App, name: &str) -> Option<ServiceOutputSnapshot> {
    let cache: &HashMap<String, ServiceOutputSnapshot> = &app.service_output_cache;
    cache.get(name).cloned()
}

/// 把元信息转成可读的「尾部 (N KiB / N MiB)」。
fn meta_label(meta: Option<&ServiceOutputMeta>) -> String {
    let Some(meta) = meta else {
        return "正在拉取".to_string();
    };
    let bytes = meta.stored_bytes;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// 读 ServiceManager 全量视图;无 ExecutionRuntime 时返回空列表。
fn current_views(app: &App) -> Vec<ServiceView> {
    app.runtime
        .execution
        .as_ref()
        .ok()
        .map(|rt| rt.services.service_view_all())
        .unwrap_or_default()
}

/// 按 max_chars 拆长字符串成行,保留原顺序。
fn wrap_text(s: &str, max_chars: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in s.chars() {
        if c == '\n' {
            out.push(std::mem::take(&mut current));
            count = 0;
            continue;
        }
        if count >= max_chars {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(c);
        count += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 截断字符串到 `max_chars` 字符,超长加 `…`。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{ClickAction, Page},
        runtime::{Route, Runtime},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use smallhead_core::execution::{smallhead_process::ProcessManager, ExecutionRuntime};

    fn app_at(path: &Path) -> App {
        let mut runtime = Runtime::new(true, 30);
        runtime.execution = Ok(std::sync::Arc::new(
            ExecutionRuntime::new(ProcessManager::with_log_root(path.to_path_buf()).unwrap())
                .unwrap(),
        ));
        App::new(runtime, Route::Large, None)
    }

    #[tokio::test]
    async fn failed_start_and_inspect_keep_recovery_error_visible() {
        let dir = tempfile::tempdir().unwrap();
        let config = smallhead_core::execution::smallhead_process::service::ServiceSpec {
            name: "recovery-test".into(),
            process: smallhead_core::execution::smallhead_process::ProcessSpec::new(
                "echo must-not-run",
            ),
            health_url: String::new(),
            identity: Default::default(),
            startup_timeout_ms: 100,
        };
        std::fs::write(
            dir.path().join("services.json"),
            serde_json::to_vec(&serde_json::json!({
                "recovery-test": {"spec": config, "state": "Ready", "execution_id": null,
                    "external": false, "desired_running": true, "detail": "old"}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut app = app_at(dir.path());
        app.current = Page::Services;
        app.processes.service_selected_name = Some("recovery-test".into());
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                app.tick().await;
                if app.status.contains("操作失败") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(app.status.contains("服务身份或清理结果未知"));
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                app.tick().await;
                if app.status.contains("尚未就绪") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(app.status.contains("历史执行证据不可用"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
        terminal
            .draw(|f| draw_detail(f, &mut app, f.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = text.split_whitespace().collect::<String>();
        assert!(compact.contains("最近操作错误:"));
        assert!(compact.contains("服务身份或清理结果未知"));
        assert!(app
            .runtime
            .execution
            .as_ref()
            .unwrap()
            .processes
            .list()
            .is_empty());
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn presets_keys_and_scrolled_hit_maps_match_rendered_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path());
        app.current = Page::Services;
        let names = app.processes.service_names();
        assert!(names.iter().any(|name| name == "laya"));
        assert!(names.iter().any(|name| name == "qwen"));
        for name in &names {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .await;
            assert_eq!(app.processes.selected_service_name().as_ref(), Some(name));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 3)).unwrap();
        terminal.draw(|f| draw_list(f, &mut app, f.area())).unwrap();
        let row: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect();
        assert!(row.contains(names.last().unwrap()));
        assert!(
            matches!(app.click_map.hit_test(2, 1), Some(ClickAction::SelectService(idx)) if idx == names.len() - 1)
        );
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn no_health_ready_and_description_timestamp_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_at(dir.path());
        let mut view = current_views(&app).remove(0);
        view.state = ServiceState::Ready;
        view.health_url.clear();
        view.description = Some("Verified process launch".into());
        view.description_updated_at = Some(std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(view_state_label(&view).0, "运行中");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| f.render_widget(build_overview_paragraph(Some(&view), None), f.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let compact = text.split_whitespace().collect::<String>();
        assert!(compact.contains("进程已启动；未配置健康/身份检查"));
        assert!(compact.contains("说明更新:1970-01-01"));
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn event_tail_is_bounded_and_renders_newest_first_without_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service-events.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'x'; EVENT_READ_BYTES as usize])
            .unwrap();
        writeln!(file).unwrap();
        for sequence in 0..700 {
            let event = ServiceEvent {
                schema_version: 1, sequence, name: "laya".into(),
                timestamp: std::time::SystemTime::now(),
                kind: smallhead_core::execution::smallhead_process::service::ServiceEventKind::DescriptionUpdated,
                operation_id: None, execution_id: None, state: None,
                reason: Some(format!("marker-{sequence:04}")),
            };
            writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        }
        let mut app = app_at(dir.path());
        assert_eq!(app.service_event_cache.len(), EVENT_CACHE_LIMIT);
        assert_eq!(app.service_event_cache.back().unwrap().sequence, 699);
        std::fs::remove_file(path).unwrap();
        let view = current_views(&app)
            .into_iter()
            .find(|v| v.name == "laya")
            .unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 240)).unwrap();
        terminal
            .draw(|f| f.render_widget(build_events_paragraph(&app, Some(&view)), f.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.find("marker-0699").unwrap() < text.find("marker-0650").unwrap());
        assert!(!text.contains("marker-0649"));
        app.shutdown().await.unwrap();
    }
}
