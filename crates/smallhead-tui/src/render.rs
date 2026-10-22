//! 纯渲染层：聊天、输入、补全、日志列表和 JSON 详情。
use crate::{
    app::{App, ClickAction, ClickMap},
    logs::FILTERS,
};
use chrono::{Local, TimeZone};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use smallhead_core::choice::ChoiceRequest;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 在 ClickMap 上注册一个区域;width/height 为 0 时跳过。
#[inline]
fn register(map: &mut ClickMap, x: u16, y: u16, w: u16, h: u16, action: ClickAction) {
    if w > 0 && h > 0 {
        map.register(x, y, w, h, action);
    }
}

fn panel(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}
fn layer_color(layer: &str) -> Color {
    match layer {
        "request" => Color::Cyan,
        "message" => Color::Green,
        "block" => Color::DarkGray,
        "tool" => Color::Yellow,
        "plan" => Color::LightYellow,
        "process" => Color::LightCyan,
        "monad" => Color::Magenta,
        "route" => Color::LightBlue,
        "error" => Color::Red,
        "response" => Color::LightGreen,
        _ => Color::White,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    // 每帧重渲前清空 hit-test 表,防止上帧坐标残留。
    app.click_map.clear();

    let area = f.area();
    if area.width < 35 || area.height < 10 {
        f.render_widget(
            Paragraph::new("窗口过小，请扩大终端（至少35×10）。Ctrl+C退出。"),
            area,
        );
        return;
    }
    let spec_page = app.current == crate::app::Page::Spec;
    let suggestions = if spec_page { vec![] } else { app.suggestions() };
    let pending_choice = if app.current.is_chat() {
        app.runtime.choices.pending().into_iter().next()
    } else {
        None
    };
    let spec_hints = if area.width < 60 {
        vec![
            Line::from(" Up/Down:Block PgUp/PgDn:Scroll"),
            Line::from(" c:Comment a:Approve Esc:Back"),
        ]
    } else {
        vec![Line::from(
            " Up/Down:Block PgUp/PgDn:Scroll c:Comment a:Approve Esc:Back",
        )]
    };
    let hint_height = if spec_page {
        spec_hints.len() as u16
    } else if pending_choice.is_some() {
        if suggestions.is_empty() {
            1
        } else {
            (suggestions.len().min(2) + 1) as u16
        }
    } else if suggestions.is_empty() {
        1
    } else {
        (suggestions.len().min(3) + 1) as u16
    };
    let buttons: [(&str, ClickAction, Color, bool); 5] = [
        (
            "[聊天 F1] ",
            ClickAction::FocusChat,
            Color::Cyan,
            app.current.is_chat(),
        ),
        (
            "[日志 F2] ",
            ClickAction::FocusLogs,
            Color::LightBlue,
            app.current.is_logs(),
        ),
        (
            "[进程 Ctrl+P] ",
            ClickAction::FocusProcesses,
            Color::LightCyan,
            app.current.is_processes(),
        ),
        (
            "[服务 Ctrl+S] ",
            ClickAction::FocusServices,
            Color::Magenta,
            app.current.is_services(),
        ),
        (
            "[Spec F9] ",
            ClickAction::FocusSpec,
            Color::Green,
            app.current == crate::app::Page::Spec,
        ),
    ];
    let mut navigation = vec![Vec::new()];
    let mut positions = Vec::new();
    let mut x = 0;
    for (label, action, color, active) in &buttons {
        let width = UnicodeWidthStr::width(*label) as u16;
        if x + width > area.width && x > 0 {
            navigation.push(Vec::new());
            x = 0;
        }
        let style = if *active {
            Style::default().bg(*color).fg(Color::Black)
        } else {
            Style::default().fg(*color)
        }
        .add_modifier(Modifier::BOLD);
        navigation
            .last_mut()
            .unwrap()
            .push(Span::styled(*label, style));
        positions.push((x, navigation.len() as u16, width, action.clone()));
        x += width;
    }
    let header_height = 1 + navigation.len() as u16;
    // Keep every navigation row and a readable body at the minimum terminal height.
    let input_height = if area.height >= header_height + 3 + hint_height + 4 {
        3
    } else {
        0
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(3),
            Constraint::Length(hint_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);
    let mut heading = vec![Line::from(vec![
        Span::styled(
            " smallhead / Monad ",
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} · {}", app.runtime.label, app.route.label()),
            Style::default().fg(Color::LightBlue),
        ),
    ])];
    heading.extend(navigation.into_iter().map(Line::from));
    f.render_widget(Paragraph::new(heading), rows[0]);
    for (x, y, width, action) in positions {
        register(
            &mut app.click_map,
            rows[0].x + x,
            rows[0].y + y,
            width,
            1,
            action,
        );
    }
    if app.current == crate::app::Page::Spec {
        if let Some(doc) = &app.spec_document {
            crate::spec_review::draw(f, rows[1], doc, &mut app.spec_view);
        } else {
            f.render_widget(Paragraph::new("暂无方案。让 Agent 先写方案供审阅，或用 /spec 路径 打开 Markdown。Esc 返回聊天。").wrap(Wrap { trim: false }).block(panel(" Spec ".into(), true)), rows[1]);
        }
    } else if app.current.is_processes() {
        crate::processes::draw(f, app, rows[1]);
    } else if app.current.is_services() {
        crate::services::draw(f, app, rows[1]);
    } else if app.current.is_logs() {
        draw_logs(f, app, rows[1], true);
    } else if area.width >= 110 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(rows[1]);
        draw_chat_with_choices(f, app, columns[0]);
        draw_logs(f, app, columns[1], false);
    } else {
        draw_chat_with_choices(f, app, rows[1]);
    }
    let hints = if spec_page {
        spec_hints
    } else if let Some(choice) = pending_choice.as_ref() {
        let mut lines = vec![choice_shortcut_hint(
            choice,
            app.choice_selection_index(choice),
        )];
        lines.extend(suggestions.iter().take(2).map(|(cmd, desc)| {
            Line::from(vec![
                Span::styled(format!(" {cmd} "), Style::default().fg(Color::Yellow)),
                Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
            ])
        }));
        lines
    } else if suggestions.is_empty() {
        vec![Line::styled(
            " 输入 / 查看命令；日志 ↑↓ 选择 · PgUp/PgDn 详情 · Ctrl+F 筛选 · End 跟随",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        suggestions
            .iter()
            .take(3)
            .map(|(cmd, desc)| {
                Line::from(vec![
                    Span::styled(format!(" {cmd} "), Style::default().fg(Color::Yellow)),
                    Span::styled(desc.to_string(), Style::default().fg(Color::Gray)),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(hints), rows[2]);
    // 补全列表(suggestions 存在时)每行注册为可点击的 RunCommand。
    // 行数 ≤ 3,第一行从 rows[2].y 开始。
    if !suggestions.is_empty() {
        let limit = if pending_choice.is_some() { 2 } else { 3 };
        for (i, (cmd, _)) in suggestions.iter().take(limit).enumerate() {
            register(
                &mut app.click_map,
                rows[2].x,
                rows[2].y + 1 + i as u16,
                rows[2].width,
                1,
                ClickAction::RunCommand((*cmd).to_string()),
            );
        }
    }
    let input_width = rows[3].width.saturating_sub(3) as usize;
    let visible = tail_width(&app.input, input_width);
    f.render_widget(
        Paragraph::new(if spec_page {
            " c 评论 · F1 聊天".into()
        } else {
            format!("{visible}▏")
        })
        .style(Style::default().fg(Color::White))
        .block(panel(
            if spec_page {
                " 方案审阅 ".into()
            } else if app.busy() {
                " 补充当前任务(Enter 提交 · Ctrl+X 取消) ".into()
            } else {
                " 输入(Enter 发送 · Ctrl+X 取消) ".into()
            },
            !app.log_focus,
        )),
        rows[3],
    );
    // 输入框点击只聚焦，发送由明确的按钮或 Enter 触发。
    if !spec_page {
        register(
            &mut app.click_map,
            rows[3].x,
            rows[3].y,
            rows[3].width,
            rows[3].height,
            ClickAction::FocusInput,
        );
    }
    // 状态栏整行 = Cancel,方便在长等待时快速取消。
    register(
        &mut app.click_map,
        rows[4].x,
        rows[4].y,
        rows[4].width,
        rows[4].height,
        ClickAction::Cancel,
    );
    // 提交/取消提示就放在输入框右侧,顶部只放 4 个页面入口。
    let elapsed = app
        .started
        .map(|s| format!(" · 等待 {:.1}s", s.elapsed().as_secs_f64()))
        .unwrap_or_default();
    // P2 #14: 静默 2s 后在状态栏附加一行 hover tooltip。
    let tooltip_span = app.tooltip.as_ref().map(|t| {
        Span::styled(
            format!(" │ ⓘ {t}"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
        )
    });
    let mut spans = vec![
        Span::styled(
            format!(" {}{}", app.status, elapsed),
            Style::default().fg(if app.busy() {
                Color::Yellow
            } else {
                Color::Green
            }),
        ),
        Span::styled(
            format!(
                " │ 响应轮数 {} · 工具 {} │ 日志 {}（淘汰 {}）",
                app.total_turns,
                app.total_tools,
                app.logs.entries.len(),
                app.logs.evicted
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(s) = tooltip_span {
        spans.push(s);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rows[4]);
}

fn choice_shortcut_hint(request: &ChoiceRequest, selected_index: usize) -> Line<'static> {
    let current = request
        .options
        .get(selected_index)
        .map(|option| option.label.as_str())
        .unwrap_or("未选择");
    let mut spans = vec![Span::styled(
        " 选择卡片：",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        "↑/↓ 高亮",
        Style::default().fg(Color::LightGreen),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        "Enter 确认",
        Style::default().fg(Color::LightGreen),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        "输入文字提交补充",
        Style::default().fg(Color::LightGreen),
    ));
    spans.push(Span::styled(
        " · 当前：",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        current.to_string(),
        Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
    ));
    Line::from(spans)
}

fn draw_chat_with_choices(f: &mut Frame, app: &mut App, area: Rect) {
    let choices = app.runtime.choices.pending();
    if choices.is_empty() {
        draw_chat_with_plan(f, app, area);
        return;
    }
    let request = &choices[0];
    let (lines, _) = choice_lines(
        request,
        area.width.saturating_sub(2).max(1) as usize,
        app.choice_selection_index(request),
    );
    let card_height = (lines.len().saturating_add(2).min(u16::MAX as usize) as u16)
        .min(area.height.saturating_sub(3));
    if area.height < 10 {
        draw_choice_card(f, app, area, request);
        return;
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(card_height.max(5))])
        .split(area);
    draw_chat_with_plan(f, app, parts[0]);
    draw_choice_card(f, app, parts[1], request);
}

fn draw_chat_with_plan(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(plan) = &app.plan else {
        draw_chat(f, app, area);
        return;
    };
    if area.height < 10 {
        draw_chat(f, app, area);
        return;
    }
    let height = (plan.snapshot.plan.len() as u16 + 2)
        .min(8)
        .min(area.height / 2);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(height), Constraint::Min(4)])
        .split(area);
    crate::task_plan::draw(f, plan, parts[0]);
    draw_chat(f, app, parts[1]);
}

fn choice_lines(
    request: &ChoiceRequest,
    width: usize,
    selected_index: usize,
) -> (Vec<Line<'static>>, Vec<std::ops::Range<usize>>) {
    let mut lines = Vec::new();
    for line in wrap_text(&request.title, width) {
        lines.push(Line::styled(
            line,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    for line in wrap_text(&request.body, width).into_iter().take(2) {
        lines.push(Line::styled(line, Style::default().fg(Color::Gray)));
    }
    let mut ranges = Vec::new();
    for (idx, option) in request.options.iter().enumerate() {
        let start = lines.len();
        let selected = idx == selected_index;
        let marker = if selected { "▶" } else { " " };
        let label_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightGreen)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD)
        };
        for line in wrap_text(&format!("{marker} {}", option.label), width) {
            lines.push(Line::styled(line, label_style));
        }
        if let Some(description) = &option.description {
            for line in wrap_text(description, width.saturating_sub(2).max(1)) {
                lines.push(Line::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Gray),
                ));
            }
        }
        ranges.push(start..lines.len());
    }
    (lines, ranges)
}

fn draw_choice_card(f: &mut Frame, app: &mut App, area: Rect, request: &ChoiceRequest) {
    let selected_index = app.choice_selection_index(request);
    let (lines, ranges) = choice_lines(
        request,
        area.width.saturating_sub(2).max(1) as usize,
        selected_index,
    );
    let height = area.height.saturating_sub(2) as usize;
    // Keep the selected option visible, including its description when it fits.
    let offset = ranges
        .get(selected_index)
        .map(|range| range.end.saturating_sub(height).min(range.start))
        .unwrap_or(0)
        .min(lines.len().saturating_sub(height));
    let title = format!(
        " 需要选择 {}/{} · ↑↓ · Enter 确认 {}{} ",
        selected_index + 1,
        request.options.len(),
        if offset > 0 { "↑" } else { "" },
        if offset + height < lines.len() {
            "↓"
        } else {
            ""
        }
    );
    f.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(height)
                .collect::<Vec<_>>(),
        )
        .block(panel(title, true)),
        area,
    );
    for (option, range) in request.options.iter().zip(ranges) {
        let start = range.start.max(offset);
        let end = range.end.min(offset + height);
        if start >= end {
            continue;
        }
        register(
            &mut app.click_map,
            area.x.saturating_add(1),
            area.y.saturating_add(1 + (start - offset) as u16),
            area.width.saturating_sub(2),
            (end - start) as u16,
            ClickAction::AnswerChoice {
                choice_id: request.id.clone(),
                option_id: option.id.clone(),
            },
        );
    }
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) {
    register(
        &mut app.click_map,
        area.x,
        area.y,
        area.width,
        area.height,
        ClickAction::ScrollChat,
    );
    let width = area.width.saturating_sub(3).max(1) as usize;
    let mut lines = Vec::new();
    let streaming = crate::app::Chat {
        role: "助手 · 生成中",
        text: app.stream_text.clone(),
    };
    let mut messages: Vec<_> = app.history.iter().collect();
    if !streaming.text.is_empty() {
        messages.insert(app.stream_history_index.min(messages.len()), &streaming);
    }
    for msg in messages {
        let color = match msg.role {
            "你" => Color::Green,
            role if role.starts_with("助手") => Color::Cyan,
            "错误" => Color::Red,
            _ => Color::DarkGray,
        };
        lines.push(Line::styled(
            format!(" {}", msg.role),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        if msg.role.starts_with("助手") {
            lines.extend(app.markdown_cache.render(&msg.text, width));
        } else {
            for line in wrap_text(&msg.text, width) {
                lines.push(Line::styled(
                    line,
                    if msg.role == "错误" {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    },
                ));
            }
        }
        lines.push(Line::raw(""));
    }
    let height = area.height.saturating_sub(2) as usize;
    let bottom = lines.len().saturating_sub(height);
    let offset = bottom.saturating_sub(app.chat_scroll as usize);
    // Slice instead of u16 Paragraph scroll to support long conversations.
    let shown: Vec<_> = lines.into_iter().skip(offset).take(height).collect();
    f.render_widget(
        Paragraph::new(shown).block(panel(
            format!(
                " 主聊天 · 会话 {} ",
                app.session_id.chars().take(8).collect::<String>()
            ),
            !app.log_focus,
        )),
        area,
    );
}
fn draw_logs(f: &mut Frame, app: &mut App, area: Rect, full: bool) {
    let horizontal = full && area.width >= 100;
    let parts = Layout::default()
        .direction(if horizontal {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints(if horizontal {
            [Constraint::Percentage(36), Constraint::Percentage(64)]
        } else {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        })
        .split(area);
    register(
        &mut app.click_map,
        parts[0].x,
        parts[0].y,
        parts[0].width,
        parts[0].height,
        ClickAction::ScrollLogs,
    );
    register(
        &mut app.click_map,
        parts[1].x,
        parts[1].y,
        parts[1].width,
        parts[1].height,
        ClickAction::ScrollDetail,
    );
    let visible;
    // list 在 panel border 内的真实可视行数，零高度时不注册数据行。
    // scrolloff 会在 ensure_visible 里自动夹到 (view_h-1)/2。
    let inner_h = parts[0].height.saturating_sub(2) as usize;
    let view_h = inner_h;
    let start;
    let end;
    {
        // 短借用:visible_count → ensure_visible 期间,app.logs 不能同时被 visible()
        // 借走 &self。所以分两步:先算 total(usize, 无借用残留),再调 ensure_visible。
        let total = app.logs.visible_count();
        // scrolloff 行为:跟随时/下翻时光标停在视窗中下,留出"未来"行视觉空间。
        app.logs.ensure_visible(view_h, total);
        visible = app.logs.visible();
        start = app.logs.view_offset;
        end = (start + view_h).min(visible.len());
    }
    let items: Vec<_> = if start < end {
        visible[start..end]
            .iter()
            .map(|e| {
                // 本地时间:用 chrono 把 epoch ms 拆成 hh:mm:ss.mmm。
                // 之前 (timestamp_ms / 3600000) % 24 是当 UTC 看的,对照真实时间不便。
                let dt = Local
                    .timestamp_millis_opt(e.record.timestamp_ms as i64)
                    .single()
                    .unwrap_or_else(|| {
                        // 时间戳异常(负值/越界)时退到 epoch 起点,避免整个 log 列表渲染失败
                        Local.timestamp_opt(0, 0).unwrap()
                    });
                let label = format!(
                    "#{:04} {:02}:{:02}:{:02}.{:03} {} / {}",
                    e.sequence,
                    dt.format("%H"),
                    dt.format("%M"),
                    dt.format("%S"),
                    // millis 直接从 naive datetime 算:timestamp_subsec_millis
                    dt.timestamp_subsec_millis(),
                    e.record.layer,
                    e.record.event
                );
                ListItem::new(Line::styled(
                    label,
                    Style::default().fg(layer_color(&e.record.layer)),
                ))
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut state = ListState::default();
    if !items.is_empty() {
        // 切片后的相对下标:highlight 跟着切片走,跟全局 visible 无关。
        let rel = app.logs.selected.saturating_sub(start);
        if rel < items.len() {
            state.select(Some(rel));
        }
    }
    f.render_stateful_widget(
        List::new(items)
            .block(panel(
                format!(
                    " 事件 · {} · {} · 余白 {} ",
                    FILTERS[app.logs.filter],
                    if app.logs.follow { "跟随" } else { "浏览" },
                    app.logs.scrolloff,
                ),
                app.log_focus,
            ))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ "),
        parts[0],
        &mut state,
    );
    // 日志列表每行 register SelectLog(abs_idx):list 内边距 1 行(panel border)。
    // 切片后的第 i 行对应 visible 全集中的第 start + i 条。
    let inner_y = parts[0].y.saturating_add(1);
    for i in 0..(end.saturating_sub(start)) {
        let abs_idx = start + i;
        register(
            &mut app.click_map,
            parts[0].x,
            inner_y + i as u16,
            parts[0].width,
            1,
            ClickAction::SelectLog(abs_idx),
        );
    }
    let (title, text) = if let Some(e) = app.logs.current() {
        (
            format!(
                " #{} {} / {} · PgUp/PgDn ",
                e.sequence, e.record.layer, e.record.event
            ),
            format!(
                "task: {}\n{}",
                e.task,
                serde_json::to_string_pretty(&e.record.data).unwrap_or_default()
            ),
        )
    } else {
        (" 详情 ".into(),"暂无事件。发送消息后选择日志；Ctrl+R 查看请求。request/provider_request 为模型接口输入，request/http_body 为实际 HTTP JSON 请求体（不含鉴权头）。".into())
    };
    let lines: Vec<_> = wrap_text(&text, parts[1].width.saturating_sub(3).max(1) as usize)
        .into_iter()
        .map(|line| {
            let color = if line.contains("\"error\"") || line.contains("\"fallback\": true") {
                Color::LightRed
            } else if line.trim_start().starts_with('"') && line.contains(':') {
                Color::LightCyan
            } else if line.contains("true") || line.contains("false") || line.contains("null") {
                Color::LightMagenta
            } else {
                Color::Gray
            };
            Line::styled(line, Style::default().fg(color))
        })
        .collect();
    let max_scroll = lines
        .len()
        .saturating_sub(parts[1].height.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((
                app.logs
                    .detail_scroll
                    .min(max_scroll.min(u16::MAX as usize) as u16),
                0,
            ))
            .block(panel(title, app.log_focus))
            .wrap(Wrap { trim: false }),
        parts[1],
    );
}
fn tail_width(text: &str, width: usize) -> String {
    let mut size = 0;
    let mut chars = vec![];
    for c in text.chars().rev() {
        size += c.width().unwrap_or(0);
        if size > width {
            break;
        }
        chars.push(c);
    }
    chars.into_iter().rev().collect()
}
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut result = vec![];
    for raw in text.lines() {
        let mut line = String::new();
        let mut size = 0;
        for c in raw.chars() {
            let w = c.width().unwrap_or(0);
            if size + w > width && !line.is_empty() {
                result.push(std::mem::take(&mut line));
                size = 0;
            }
            line.push(c);
            size += w;
        }
        result.push(line);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Page;

    #[tokio::test]
    async fn chat_markdown_handles_streaming_resize_and_scroll_without_changing_history() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        let source = "## Gradient\n\n**Important**: `alpha`\n\n1. Read\n2. Calculate\n\n```text\n    result = 0.496\n```";
        app.history = vec![
            crate::app::Chat {
                role: "你",
                text: "Keep **my literal input**".into(),
            },
            crate::app::Chat {
                role: "助手",
                text: source.into(),
            },
        ];
        app.stream_text = "**Streaming**: `0.460`".into();
        app.stream_history_index = 2;
        for width in [35, 60, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
            terminal.draw(|f| draw_chat(f, &mut app, f.area())).unwrap();
            let screen = screen_text(&terminal);
            assert!(screen.contains("**my literal input**"));
            assert!(!screen.contains("**Important**"));
            assert!(!screen.contains("```"));
            assert!(screen.contains("result = 0.496"));
            assert!(screen.contains("Streaming: 0.460"));
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol() == "I" && cell.modifier.contains(Modifier::BOLD)));
            assert_eq!(app.history[1].text, source);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(35, 8)).unwrap();
        terminal.draw(|f| draw_chat(f, &mut app, f.area())).unwrap();
        assert!(screen_text(&terminal).contains("Streaming: 0.460"));
        app.chat_scroll = u16::MAX;
        terminal.draw(|f| draw_chat(f, &mut app, f.area())).unwrap();
        assert!(screen_text(&terminal).contains("Keep **my literal input**"));
        assert!(!screen_text(&terminal).contains("Streaming: 0.460"));
    }

    #[tokio::test]
    async fn live_response_stays_before_newer_user_message() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.history = vec![
            crate::app::Chat {
                role: "你",
                text: "First request".into(),
            },
            crate::app::Chat {
                role: "你",
                text: "Second request".into(),
            },
        ];
        app.stream_history_index = 1;
        app.stream_text = "Visible partial response".into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| draw_chat(frame, &mut app, frame.area()))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(
            text.find("First request").unwrap() < text.find("Visible partial response").unwrap()
        );
        assert!(
            text.find("Visible partial response").unwrap() < text.find("Second request").unwrap()
        );
    }

    fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn five_navigation_buttons_wrap_and_match_every_clickable_cell() {
        let buttons = [
            (Page::Chat, ClickAction::FocusChat, "[聊天 F1] "),
            (Page::Logs, ClickAction::FocusLogs, "[日志 F2] "),
            (
                Page::Processes,
                ClickAction::FocusProcesses,
                "[进程 Ctrl+P] ",
            ),
            (Page::Services, ClickAction::FocusServices, "[服务 Ctrl+S] "),
            (Page::Spec, ClickAction::FocusSpec, "[Spec F9] "),
        ];
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        for width in [35, 60, 80, 120] {
            for height in [10, 24] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                for (active_page, _, _) in &buttons {
                    app.current = *active_page;
                    terminal.draw(|f| draw(f, &mut app)).unwrap();
                    let mut last_row = 0;
                    let mut targets = Vec::new();
                    for (page, action, label) in &buttons {
                        let cells: Vec<_> = (0..height)
                            .flat_map(|y| (0..width).map(move |x| (x, y)))
                            .filter(|(x, y)| {
                                app.click_map.hit_test(*x, *y).is_some_and(|a| {
                                    std::mem::discriminant(&a) == std::mem::discriminant(action)
                                })
                            })
                            .collect();
                        assert_eq!(
                            cells.len(),
                            label.width(),
                            "{width}x{height} {page:?}: {cells:?}"
                        );
                        let (x, y) = cells[0];
                        assert!(y > 0 && y < height);
                        assert!(x as usize + cells.len() <= width as usize);
                        last_row = last_row.max(y);
                        for (offset, &(cx, cy)) in cells.iter().enumerate() {
                            assert_eq!((cx, cy), (x + offset as u16, y));
                            let bg = terminal.backend().buffer()[(cx, cy)].bg;
                            // Ratatui resets the trailing cell of a double-width glyph.
                            let continuation = cx > x
                                && terminal.backend().buffer()[(cx - 1, cy)].symbol().width() == 2;
                            if !continuation {
                                assert_eq!(
                                    bg != Color::Reset,
                                    page == active_page,
                                    "{width}x{height}: wrong highlight at {cx},{cy}"
                                );
                            }
                        }
                        let visible: String = cells
                            .iter()
                            .map(|&(x, y)| terminal.backend().buffer()[(x, y)].symbol())
                            .collect();
                        assert_eq!(visible.replace(' ', ""), label.replace(' ', ""));
                        targets.push((*page, cells[0], *cells.last().unwrap()));
                    }
                    assert_eq!(last_row, if width == 35 { 2 } else { 1 });
                    for y in 1..=last_row {
                        assert!(
                            (0..width).any(|x| terminal.backend().buffer()[(x, y)].symbol() == "[")
                        );
                        assert!(
                            app.click_map.hit_test(width, y).is_none(),
                            "overflow click at width {width}"
                        );
                    }
                    for (page, start, end) in targets {
                        app.on_click(start.0, start.1).await;
                        assert_eq!(app.current, page);
                        app.on_click(end.0, end.1).await;
                        assert_eq!(app.current, page);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn spec_page_renders_revision_anchors_replies_and_preserves_inline_input() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.command("/choice-demo").await;
        app.input = "CHAT_DRAFT_MUST_STAY_HIDDEN".into();
        let source = app
            .spec_broker
            .publish(None, "Review title", "# Stable\n\nOld paragraph", None)
            .unwrap();
        let mut doc = crate::spec_review::SpecDocument::from_document(&source).unwrap();
        let id = doc
            .add_comment(1, doc.blocks[0].id, "Historical question")
            .unwrap();
        doc.reply(1, id, "Answer retained").unwrap();
        doc.add_comment(1, doc.blocks[1].id, "Removed block question")
            .unwrap();
        let updated = app
            .spec_broker
            .publish(
                Some(&source.id),
                "Review title",
                "# Stable\n\nNew paragraph",
                Some(1),
            )
            .unwrap();
        doc.sync_from_document(&updated).unwrap();
        app.spec_document = Some(doc);
        app.current = Page::Spec;
        for width in [35, 60, 80, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let rendered = screen_text(&terminal);
            let compact: String = rendered
                .chars()
                .filter(|c| !c.is_whitespace() && !"│┌┐└┘─".contains(*c))
                .collect();
            for expected in [
                "Spec r2 REVIEW",
                "Comment 1 r1",
                "Reply: Answer retained",
                "Comments on earlier blocks",
                "Old paragraph",
                "New paragraph",
            ] {
                assert!(
                    compact.contains(&expected.replace(' ', "")),
                    "{width}: missing {expected}:\n{rendered}"
                );
            }
            assert!(!rendered.contains("CHAT_DRAFT_MUST_STAY_HIDDEN"));
            for y in 0..40 {
                for x in 0..width {
                    assert!(!matches!(
                        app.click_map.hit_test(x, y),
                        Some(
                            ClickAction::AnswerChoice { .. }
                                | ClickAction::FocusInput
                                | ClickAction::RunCommand(_)
                        )
                    ));
                }
            }
            assert_eq!(app.runtime.choices.pending().len(), 1);
            let doc = app.spec_document.as_ref().unwrap();
            app.spec_view
                .handle_key(crossterm::event::KeyCode::Char('c').into(), doc);
            app.spec_view.input = Some("INLINE_REVIEW_DRAFT".into());
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            assert!(screen_text(&terminal).contains("INLINE_REVIEW_DRAFT"));
            assert_eq!(app.spec_view.input_revision, Some(2));
            app.spec_view.finish_comment();
        }
    }

    #[tokio::test]
    async fn empty_spec_and_nonchat_pages_do_not_expose_pending_choice_targets() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.command("/choice-demo").await;
        for width in [35, 60, 80, 120] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
            for page in [Page::Spec, Page::Logs, Page::Processes, Page::Services] {
                app.current = page;
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                if page == Page::Spec {
                    assert!(screen_text(&terminal).replace(' ', "").contains("暂无方案"));
                }
                for y in 0..24 {
                    for x in 0..width {
                        assert!(!matches!(
                            app.click_map.hit_test(x, y),
                            Some(ClickAction::AnswerChoice { .. })
                        ));
                    }
                }
            }
        }
        assert_eq!(app.runtime.choices.pending().len(), 1);
    }

    #[tokio::test]
    async fn long_plan_keeps_active_step_visible_and_choice_keyboard_accessible() {
        use smallhead_core::plan::{PlanStatus, PlanStep, TaskPlan};
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.plan = Some(crate::task_plan::PlanView {
            snapshot: TaskPlan {
                task_id: "render".into(),
                explanation: None,
                plan: (0..20)
                    .map(|i| PlanStep {
                        step: if i == 15 {
                            "当前正在验证的步骤".into()
                        } else {
                            format!("步骤 {i}")
                        },
                        status: if i == 15 {
                            PlanStatus::InProgress
                        } else if i < 15 {
                            PlanStatus::Completed
                        } else {
                            PlanStatus::Pending
                        },
                    })
                    .collect(),
            },
            phase: crate::task_plan::PlanPhase::Running,
        });
        app.command("/choice-demo").await;
        for (width, height) in [(35, 10), (60, 24), (100, 36), (150, 40)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if height >= 36 {
                let compact = rendered.replace(' ', "");
                assert!(compact.contains("当前正在验证的步骤"), "{rendered}");
                assert!(rendered.contains("15/20"), "{rendered}");
                assert!(compact.contains("覆盖"), "{rendered}");
            }
        }
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Down,
        ))
        .await;
        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ))
        .await;
        assert!(app.runtime.choices.pending().is_empty());
        app.command("/plan").await;
        assert!(app.history.last().unwrap().text.contains("步骤 19"));
    }
    #[test]
    fn chinese_wrap_and_narrow_terminal_render() {
        assert_eq!(wrap_text("你好世界", 4), vec!["你好", "世界"]);
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.input = "/route".into();
        for (width, height) in [(30, 8), (80, 24), (150, 40)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            app.current = Page::Logs;
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            app.current = Page::Chat;
        }
    }
    #[test]
    fn log_highlight_and_hit_map_match_visible_events() {
        for margin in [0, 5] {
            let mut app = App::new(
                crate::runtime::Runtime::new(true, 30),
                crate::runtime::Route::Large,
                None,
            );
            app.logs.set_scrolloff(margin);
            for _ in 0..100 {
                app.logs.push(
                    "t",
                    smallhead_core::diagnostics::DiagnosticEvent {
                        timestamp_ms: 0,
                        layer: "tool".into(),
                        event: "e".into(),
                        data: serde_json::json!({}),
                    },
                );
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|f| draw_logs(f, &mut app, Rect::new(0, 0, 80, 24), true))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut highlights = 0;
            for y in 0..24 {
                let line: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                if let Some(ClickAction::SelectLog(index)) = app.click_map.hit_test(0, y) {
                    let sequence = app.logs.visible()[index].sequence;
                    assert!(line.contains(&format!("#{sequence:04}")), "{line}");
                    if line.contains('▶') {
                        highlights += 1;
                        assert_eq!(index, app.logs.selected);
                        assert_eq!(sequence, app.logs.current().unwrap().sequence);
                    }
                }
            }
            assert_eq!(highlights, 1);
        }
    }
    #[tokio::test]
    async fn visible_buttons_dispatch_and_input_click_only_focuses() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.input = "/route laya".into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..100).map(|x| buffer[(x, 1)].symbol()).collect();
        // 顶部 4 个页面入口。发送/取消的提示搬到输入框标题里,这里不再检查。
        for label in ["[聊天F1]", "[日志F2]", "[进程Ctrl+P]", "[服务Ctrl+S]"] {
            assert!(text.replace(' ', "").contains(label), "{text}");
        }
        // 4 个按钮都必须有对应的 ClickAction 命中区;否则鼠标无法点。
        for action in [
            ClickAction::FocusChat,
            ClickAction::FocusLogs,
            ClickAction::FocusProcesses,
            ClickAction::FocusServices,
        ] {
            assert!(
                (0..100)
                    .any(|x| matches!(app.click_map.hit_test(x, 1), Some(a) if std::mem::discriminant(&a) == std::mem::discriminant(&action))),
                "顶部缺失按钮命中区: {action:?}"
            );
        }
        // 默认 Chat 激活:点击 Chat 按钮仍是 Chat,不应切走。
        let chat_x = (0..100)
            .find(|x| matches!(app.click_map.hit_test(*x, 1), Some(ClickAction::FocusChat)))
            .unwrap();
        app.on_click(chat_x, 1).await;
        assert!(app.current.is_chat());
        let log_x = (0..100)
            .find(|x| matches!(app.click_map.hit_test(*x, 1), Some(ClickAction::FocusLogs)))
            .unwrap();
        app.on_click(log_x, 1).await;
        assert!(app.current.is_logs());
        app.on_click(chat_x, 1).await;
        assert!(!app.current.is_logs());
        assert!(app.current.is_chat());
        // 从当前绘制的命中表查找真实输入区域。
        let (x, y) = (0..24)
            .flat_map(|y| (0..100).map(move |x| (x, y)))
            .find(|(x, y)| {
                matches!(
                    app.click_map.hit_test(*x, *y),
                    Some(ClickAction::FocusInput)
                )
            })
            .unwrap();
        app.on_click(x, y).await;
        assert_eq!(app.input, "/route laya");
        assert_eq!(app.route, crate::runtime::Route::Large);
        // 提交/取消已经搬到输入框标题(可见但不可点击),不再有顶部 Submit 按钮。
        // 改用 Enter 键触发提交,验证 route 命令真的跑通。
        app.key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
        .await;
        assert_eq!(app.route, crate::runtime::Route::Laya);
        assert!(app.input.is_empty());
        assert!(!app.busy());
    }

    /// Commit 3: 顶部 4 个按钮当前页应当用反色背景突出,其它按钮仅前景色。
    /// 验证在 Chat / Processes / Services 三种状态下,反色格子数 ≥ 1 且都集中
    /// 在对应按钮区域内,不会"全亮"或"全灰"。
    #[tokio::test]
    async fn top_bar_marks_current_page_with_inverted_colors() {
        use crate::app::Page;
        for page in [Page::Chat, Page::Processes, Page::Services] {
            let mut app = App::new(
                crate::runtime::Runtime::new(true, 30),
                crate::runtime::Route::Large,
                None,
            );
            app.current = page;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let mut inverted = 0usize;
            for x in 0..120 {
                let cell = &buffer[(x, 1)];
                // TestBackend 的默认背景是 Color::Reset;只要不是 Reset,就视为显式高亮。
                if let Some(bg) = cell.style().bg {
                    if bg != Color::Reset {
                        inverted += 1;
                    }
                }
            }
            assert!(
                inverted > 0 && inverted < 120,
                "页面 {page:?} 顶部高亮格子数={inverted} 应在 1..120 范围"
            );
            // 第一个高亮格的 x 坐标。看其后面 30 列是否包含当前页按钮 label。
            let first_x = (0..120)
                .find(|x| {
                    let bg = buffer[(*x, 1)].style().bg;
                    bg.is_some_and(|b| b != Color::Reset)
                })
                .unwrap();
            let text: String = (first_x..first_x + 30)
                .map(|x| buffer[(x, 1)].symbol())
                .collect();
            // 取第一个 '[' 之后的内容(忽略起首的空格 / 全角字符宽度差异)。
            let after_bracket = text.find('[').map(|i| &text[i..]).unwrap_or(&text);
            let expected = match page {
                Page::Chat => "[聊",
                Page::Logs => "[日",
                Page::Processes => "[进",
                Page::Services => "[服",
                Page::Spec => "[Spec",
            };
            assert!(
                after_bracket.starts_with(expected),
                "高亮区 {first_x}.. 应以 {expected:?} 开头 实际: {text:?}"
            );
        }
    }

    #[tokio::test]
    async fn long_choice_selection_scrolls_and_visible_wrapped_rows_match_clicks() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.command("/choice-demo").await;
        let mut request = app.runtime.choices.pending().remove(0);
        for (idx, option) in request.options.iter_mut().enumerate() {
            option.label = format!("选项{idx}：先把 smallhead 纳入方案，再生成介绍");
            option.description = Some(
                "说明文字很长，需要换行展示，并且必须保证最后一个选项也能通过键盘选择和鼠标点击。"
                    .repeat(4),
            );
        }
        for width in [35, 60, 100] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
            for selected in 0..request.options.len() {
                app.choice_selection = selected;
                app.click_map.clear();
                terminal
                    .draw(|f| draw_choice_card(f, &mut app, f.area(), &request))
                    .unwrap();
                let rendered = screen_text(&terminal);
                assert!(rendered.contains('▶'), "{rendered}");
                assert!(
                    rendered
                        .replace(' ', "")
                        .contains(&format!("选项{selected}")),
                    "{rendered}"
                );
                let (_, ranges) = choice_lines(&request, width as usize - 2, selected);
                let height = 10;
                let offset = ranges[selected]
                    .end
                    .saturating_sub(height)
                    .min(ranges[selected].start);
                for row in 1..11 {
                    let expected = ranges
                        .iter()
                        .position(|range| range.contains(&(offset + row - 1)));
                    let actual = app.click_map.hit_test(1, row as u16);
                    match expected {
                        Some(idx) => assert!(
                            matches!(actual, Some(ClickAction::AnswerChoice { option_id, .. }) if option_id == request.options[idx].id)
                        ),
                        None => assert!(actual.is_none()),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn choice_demo_renders_clickable_option_card() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.command("/choice-demo").await;
        assert_eq!(app.runtime.choices.pending().len(), 1);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let target = (0..24)
            .flat_map(|y| (0..100).map(move |x| (x, y)))
            .find(|(x, y)| {
                matches!(
                    app.click_map.hit_test(*x, *y),
                    Some(ClickAction::AnswerChoice { ref option_id, .. }) if option_id == "overwrite"
                )
            })
            .expect("overwrite option should be clickable");
        app.on_click(target.0, target.1).await;

        assert!(app.runtime.choices.pending().is_empty());
        assert!(app
            .history
            .iter()
            .any(|entry| entry.text.contains("选择卡片已回答：覆盖")));
    }

    #[tokio::test]
    async fn choice_demo_accepts_arrow_selection_and_enter() {
        let mut app = App::new(
            crate::runtime::Runtime::new(true, 30),
            crate::runtime::Route::Large,
            None,
        );
        app.command("/choice-demo").await;
        assert_eq!(app.runtime.choices.pending().len(), 1);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..24)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("Enter确认"), "{rendered}");
        assert!(compact.contains("▶覆盖"), "{rendered}");

        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Down,
        ))
        .await;
        assert_eq!(app.choice_selection, 1);

        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..24)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("当前：追加"), "{rendered}");
        assert!(compact.contains("▶追加"), "{rendered}");

        app.key(crossterm::event::KeyEvent::from(
            crossterm::event::KeyCode::Enter,
        ))
        .await;

        assert!(app.runtime.choices.pending().is_empty());
        assert!(app
            .history
            .iter()
            .any(|entry| entry.text.contains("选择卡片已回答：追加")));
    }
}
