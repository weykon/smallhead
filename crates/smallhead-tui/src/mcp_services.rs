//! MCP connections remain owned by the shared SDK manager, not ServiceManager.
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use smallhead_mcp::{
    ConnectionSnapshot, ConnectionStatus, McpConfig, McpManager, McpToolInfo, TransportKind,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr;

type ActionResult = Result<Option<(String, Vec<McpToolInfo>)>, String>;

struct Job {
    label: String,
    handle: JoinHandle<ActionResult>,
}

#[derive(Clone, Default)]
pub struct ConfigSource {
    path: Option<PathBuf>,
    error: Option<String>,
}

pub fn default_manager(_mock: bool) -> Arc<McpManager> {
    Arc::new(McpManager::new(McpConfig::default()).expect("empty MCP config is valid"))
}

pub fn load_default(mock: bool) -> (Arc<McpManager>, ConfigSource) {
    if cfg!(test) && mock {
        return (default_manager(true), ConfigSource::default());
    }
    let path = std::env::var_os("SMALLHEAD_MCP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".smallhead/mcp.json"))
        });
    match path {
        Some(path) => load_path(path),
        None => (
            default_manager(false),
            ConfigSource {
                path: None,
                error: Some("未设置 HOME 或 SMALLHEAD_MCP_CONFIG".into()),
            },
        ),
    }
}

fn load_path(path: PathBuf) -> (Arc<McpManager>, ConfigSource) {
    match McpManager::load(&path) {
        Ok(manager) => (
            Arc::new(manager),
            ConfigSource {
                path: Some(path),
                error: None,
            },
        ),
        Err(error) => {
            let missing =
                std::fs::metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            (
                default_manager(false),
                ConfigSource {
                    path: Some(path),
                    error: Some(if missing {
                        "MCP 配置文件尚不存在；创建后按 R 重载".into()
                    } else {
                        format!("MCP 配置读取失败: {error}")
                    }),
                },
            )
        }
    }
}

async fn run_action(
    manager: Arc<McpManager>,
    action: &str,
    name: Option<&str>,
    path: Option<PathBuf>,
) -> ActionResult {
    match action {
        "list" => {
            manager.list().await;
        }
        "connect" => {
            manager
                .connect(name.unwrap_or_default())
                .await
                .map_err(|e| e.to_string())?;
        }
        "disconnect" => {
            manager
                .disconnect(name.unwrap_or_default())
                .await
                .map_err(|e| e.to_string())?;
        }
        "reconnect" => {
            manager
                .reconnect(name.unwrap_or_default())
                .await
                .map_err(|e| e.to_string())?;
        }
        "tools" => {
            let name = name.unwrap_or_default();
            let tools = manager.tools(name).await.map_err(|e| e.to_string())?;
            return Ok(Some((name.to_string(), tools)));
        }
        "reload" => {
            let path = path.ok_or("没有可重载的 MCP 配置路径")?;
            manager.reload(path).await.map_err(|e| e.to_string())?;
        }
        _ => return Err("未知 MCP 操作".into()),
    }
    Ok(None)
}

#[derive(Default)]
pub struct McpView {
    pub selected: Option<String>,
    detail_focus: bool,
    scroll: u16,
    jobs: Vec<Job>,
    notice: String,
    source: ConfigSource,
    snapshots: Vec<ConnectionSnapshot>,
    tools: HashMap<String, Vec<McpToolInfo>>,
    poll: Option<JoinHandle<Vec<ConnectionSnapshot>>>,
    next_poll: Option<Instant>,
}

impl McpView {
    pub fn new(source: ConfigSource) -> Self {
        Self {
            source,
            ..Default::default()
        }
    }
    pub fn command(&mut self, command: &str, manager: Arc<McpManager>) -> Result<(), String> {
        let words: Vec<_> = command.split_whitespace().collect();
        let action = words.first().copied().unwrap_or("list");
        let name = words.get(1).copied();
        let valid = match action {
            "list" | "reload" => words.len() <= 1,
            "connect" | "disconnect" | "reconnect" | "tools" => words.len() == 2,
            _ => false,
        };
        if !valid {
            return Err(
                "/mcp list | connect NAME | disconnect NAME | reconnect NAME | tools NAME | reload"
                    .into(),
            );
        }
        if self.jobs.len() >= 16 {
            return Err("MCP 请求较多，请等待当前操作完成".into());
        }
        if let Some(name) = name {
            self.selected = Some(name.to_string());
            self.scroll = 0;
        }
        if action == "tools" {
            self.detail_focus = true;
        }
        let label = words.join(" ");
        let label = if label.is_empty() {
            "list".into()
        } else {
            label
        };
        let action = action.to_string();
        let name = name.map(str::to_string);
        self.notice = format!("等待 MCP {label}");
        let path = self.source.path.clone();
        self.jobs.push(Job {
            label,
            handle: tokio::spawn(async move {
                run_action(manager, &action, name.as_deref(), path).await
            }),
        });
        Ok(())
    }

    pub fn key(&mut self, key: KeyEvent, manager: Arc<McpManager>) -> bool {
        let rows = server_rows(self);
        let index = self
            .selected
            .as_ref()
            .and_then(|name| rows.iter().position(|r| &r.name == name));
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                if self.detail_focus {
                    self.scroll = self
                        .scroll
                        .saturating_add_signed(if key.code == KeyCode::Up { -1 } else { 1 });
                } else if !rows.is_empty() {
                    let next = index
                        .map(|i| {
                            i.saturating_add_signed(if key.code == KeyCode::Up { -1 } else { 1 })
                                .min(rows.len() - 1)
                        })
                        .unwrap_or(0);
                    self.selected = Some(rows[next].name.clone());
                    self.scroll = 0;
                }
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => {
                if self.selected.is_none() {
                    self.selected = rows.first().map(|row| row.name.clone());
                }
                self.detail_focus = !self.detail_focus;
            }
            KeyCode::Esc if self.detail_focus => self.detail_focus = false,
            KeyCode::PageUp | KeyCode::PageDown => {
                self.detail_focus = true;
                self.scroll = self
                    .scroll
                    .saturating_add_signed(if key.code == KeyCode::PageUp { -10 } else { 10 });
            }
            KeyCode::Char('c' | 'x' | 'r' | 't') => {
                let name = self
                    .selected
                    .clone()
                    .or_else(|| rows.first().map(|row| row.name.clone()));
                if let Some(name) = name {
                    let action = match key.code {
                        KeyCode::Char('c') => "connect",
                        KeyCode::Char('x') => "disconnect",
                        KeyCode::Char('r') => "reconnect",
                        _ => "tools",
                    };
                    if let Err(error) = self.command(&format!("{action} {name}"), manager) {
                        self.notice = error;
                    }
                    if action == "tools" {
                        self.detail_focus = true;
                    }
                } else {
                    self.notice = "尚未配置 MCP 服务".into();
                }
            }
            KeyCode::Char('R') => {
                if let Err(error) = self.command("reload", manager) {
                    self.notice = error;
                }
            }
            // Do not let ordinary service actions operate on hidden selection.
            KeyCode::Char('s' | 'd') => self.notice = "MCP: c 连接 · x 断开 · r 重连".into(),
            _ => return false,
        }
        true
    }

    pub async fn tick(&mut self, manager: Arc<McpManager>) -> Vec<(String, Result<(), String>)> {
        let mut finished = Vec::new();
        let mut index = 0;
        while index < self.jobs.len() {
            if !self.jobs[index].handle.is_finished() {
                index += 1;
                continue;
            }
            let job = self.jobs.remove(index);
            let result = job
                .handle
                .await
                .map_err(|e| format!("后台任务异常: {e}"))
                .and_then(|r| r);
            self.notice = match &result {
                Ok(_) => format!("MCP {} 完成", job.label),
                Err(error) => format!("MCP {} 失败: {error}", job.label),
            };
            if job.label == "reload" {
                self.source.error = result.as_ref().err().cloned();
                if result.is_ok() {
                    self.tools.clear();
                }
            }
            if let Ok(Some((name, tools))) = &result {
                self.tools.insert(name.clone(), tools.clone());
            }
            self.next_poll = None;
            finished.push((job.label, result.map(|_| ())));
        }
        if self.poll.as_ref().is_some_and(|poll| poll.is_finished()) {
            if let Ok(snapshots) = self.poll.take().unwrap().await {
                self.tools.retain(|name, _| {
                    snapshots.iter().any(|snapshot| {
                        &snapshot.name == name && snapshot.status == ConnectionStatus::Ready
                    })
                });
                self.snapshots = snapshots;
            }
        }
        if self.poll.is_none() && self.next_poll.is_none_or(|next| Instant::now() >= next) {
            self.poll = Some(tokio::spawn(async move { manager.list().await }));
            self.next_poll = Some(Instant::now() + Duration::from_millis(500));
        }
        finished
    }

    pub async fn shutdown(&mut self, manager: Arc<McpManager>) -> Result<(), String> {
        // Cancel UI request futures before the manager closes every connection.
        for job in &self.jobs {
            job.handle.abort();
        }
        for job in self.jobs.drain(..) {
            let _ = job.handle.await;
        }
        if let Some(poll) = self.poll.take() {
            poll.abort();
            let _ = poll.await;
        }
        manager
            .disconnect_all()
            .await
            .map_err(|error| error.to_string())
    }
}

struct ServerRow {
    name: String,
    state: String,
    transport: String,
    detail: Vec<String>,
}

fn server_rows(view: &McpView) -> Vec<ServerRow> {
    view.snapshots
        .iter()
        .map(|snapshot| {
            let state = match snapshot.status {
                ConnectionStatus::Disconnected => "未连接",
                ConnectionStatus::Connecting => "连接中",
                ConnectionStatus::Ready => "MCP 就绪",
                ConnectionStatus::Failed => "连接失败",
                ConnectionStatus::Disabled => "已禁用",
            }
            .to_string();
            let transport = match snapshot.transport {
                TransportKind::Stdio => "stdio",
                TransportKind::Http => "HTTP",
            }
            .to_string();
            let mut detail = vec![
                snapshot.name.clone(),
                format!("{transport} · {state}"),
                format!("已发现 {} 个工具", snapshot.tool_count),
            ];
            if let Some(description) = &snapshot.description {
                detail.push(description.clone());
            }
            if let Some(error) = &snapshot.last_error {
                detail.push(format!("错误: {error}"));
            }
            if let Some(tools) = view.tools.get(&snapshot.name) {
                for tool in tools {
                    detail.push(String::new());
                    detail.push(tool.name.clone());
                    if let Some(description) = &tool.description {
                        detail.push(description.clone());
                    }
                    if let Ok(schema) = serde_json::to_string_pretty(&tool.input_schema) {
                        detail.extend(schema.lines().map(str::to_string));
                    }
                }
            } else if snapshot.status == ConnectionStatus::Ready {
                detail.push("t 查看工具与参数".into());
            }
            ServerRow {
                name: snapshot.name.clone(),
                state,
                transport,
                detail,
            }
        })
        .collect()
}

pub fn draw(f: &mut Frame, view: &mut McpView, _manager: &Arc<McpManager>, area: Rect) {
    let rows = server_rows(view);
    let controls =
        "MCP · m 服务 · ↑↓ 选择 · Enter/Tab 详情\nc 连接 · x 断开 · r 重连 · t 工具 · R 重载";
    let control_height = controls
        .lines()
        .map(|line| line.width().div_ceil(usize::from(area.width.max(1))))
        .sum::<usize>()
        .min(usize::from(area.height.saturating_sub(3))) as u16;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(control_height),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(controls).wrap(Wrap { trim: false }),
        layout[0],
    );
    let selected = view
        .selected
        .as_ref()
        .and_then(|name| rows.iter().position(|r| &r.name == name));
    let (list_area, detail_area) = if area.width < 100 {
        if view.detail_focus {
            (None, Some(layout[1]))
        } else {
            (Some(layout[1]), None)
        }
    } else {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(layout[1]);
        (Some(columns[0]), Some(columns[1]))
    };
    if let Some(area) = list_area {
        if rows.is_empty() {
            let path = view
                .source
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未指定".into());
            f.render_widget(
                Paragraph::new(format!(
                    "未配置 MCP 服务\n配置: {path}\n{}\n/mcp reload 重载配置",
                    view.source.error.as_deref().unwrap_or("")
                ))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" MCP 服务 ")),
                area,
            );
        } else {
            let items: Vec<_> = rows
                .iter()
                .map(|row| {
                    ListItem::new(format!("{} · {} · {}", row.name, row.state, row.transport))
                })
                .collect();
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" MCP 服务 "))
                    .highlight_style(Style::default().fg(Color::Cyan))
                    .highlight_symbol("> "),
                area,
                &mut ListState::default().with_selected(selected),
            );
        }
    }
    if let Some(area) = detail_area {
        let lines: Vec<Line> = selected
            .and_then(|index| rows.get(index))
            .map(|row| row.detail.iter().cloned().map(Line::from).collect())
            .unwrap_or_else(|| vec![Line::from("选择 MCP 服务查看连接状态与工具")]);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((view.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" MCP 详情 · Esc 返回 "),
                ),
            area,
        );
    }
    let status = if view.jobs.is_empty() {
        view.notice.clone()
    } else {
        format!("{} · {} 个操作处理中", view.notice, view.jobs.len())
    };
    let status = match &view.source.error {
        Some(error) => format!("{error}\n{status}"),
        None => status,
    };
    f.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), layout[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{App, Page},
        runtime::{Route, Runtime},
    };
    use crossterm::event::KeyModifiers;

    async fn finish_jobs(
        view: &mut McpView,
        manager: &Arc<McpManager>,
    ) -> Vec<(String, Result<(), String>)> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut results = Vec::new();
            loop {
                results.extend(view.tick(manager.clone()).await);
                if view.jobs.is_empty() {
                    break results;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("MCP UI jobs did not finish")
    }

    #[tokio::test]
    async fn bad_config_notice_reload_and_keyboard_connection_failure_preserve_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "{invalid").unwrap();
        let (manager, source) = load_path(path.clone());
        assert!(source.error.is_some());
        let mut view = McpView::new(source);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 18)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut view, &manager, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text
            .split_whitespace()
            .collect::<String>()
            .contains("配置读取失败"));
        let config = serde_json::json!({"mcpServers": {"alpha": {
            "command": dir.path().join("missing-executable"), "connect_timeout_ms": 500
        }}})
        .to_string();
        std::fs::write(&path, &config).unwrap();
        view.command("reload", manager.clone()).unwrap();
        assert!(finish_jobs(&mut view, &manager)
            .await
            .iter()
            .all(|(_, result)| result.is_ok()));
        assert!(view.source.error.is_none());
        assert_eq!(
            manager.snapshots()[0].status,
            ConnectionStatus::Disconnected,
            "reload must not autostart commands"
        );
        view.snapshots = manager.snapshots();
        assert!(view.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            manager.clone()
        ));
        assert_eq!(view.selected.as_deref(), Some("alpha"));
        assert!(view.key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            manager.clone()
        ));
        assert!(finish_jobs(&mut view, &manager).await[0].1.is_err());
        assert!(view.notice.contains("失败"));
        let snapshots = manager.snapshots();
        assert_eq!(snapshots[0].name, "alpha");
        assert_eq!(snapshots[0].status, ConnectionStatus::Failed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
        view.snapshots = snapshots;
        assert!(view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            manager.clone()
        ));
        terminal
            .draw(|frame| draw(frame, &mut view, &manager, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("alpha"));
        assert!(text
            .split_whitespace()
            .collect::<String>()
            .contains("连接失败"));
        assert!(view.key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            manager.clone()
        ));
        assert!(finish_jobs(&mut view, &manager).await[0].1.is_ok());
        assert_eq!(
            manager.snapshots()[0].status,
            ConnectionStatus::Disconnected
        );
        view.shutdown(manager).await.unwrap();
    }

    #[tokio::test]
    async fn services_m_toggles_mcp_and_commands_remain_typeable() {
        let mut app = App::new(Runtime::new(true, 30), Route::Large, None);
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.current, Page::Services);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal
            .draw(|frame| crate::services::draw(frame, &mut app, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("m MCP"),
            "ordinary services must expose the MCP toggle"
        );
        app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
            .await;
        assert!(app.services_mcp);
        for c in "/mcp list".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(app.input, "/mcp list");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.input.is_empty());
        assert!(app.services_mcp);
        app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
            .await;
        assert!(!app.services_mcp);
        app.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_connect_finishes_in_background_and_remains_visible() {
        let manager = default_manager(true);
        let mut view = McpView::default();
        view.command("connect not-configured", manager.clone())
            .unwrap();
        assert_eq!(view.jobs.len(), 1);
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let finished = view.tick(manager.clone()).await;
                if !finished.is_empty() {
                    break finished;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(finished[0].1.is_err());
        assert!(view.jobs.is_empty());
        assert!(view.notice.contains("失败"));
        assert!(view.notice.contains("not-configured"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut view, &manager, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("not-configured"));
        view.shutdown(manager).await.unwrap();
    }

    #[tokio::test]
    async fn mcp_empty_view_renders_controls_at_narrow_and_wide_sizes() {
        let manager = default_manager(true);
        let mut view = McpView::default();
        for (width, height) in [(1, 1), (40, 16), (80, 24), (140, 35)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, &mut view, &manager, frame.area()))
                .unwrap();
            if width > 1 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("MCP"));
                assert!(text.contains("c "));
                assert!(text.contains("m "));
            }
        }
        assert!(view.key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            manager.clone()
        ));
        assert!(
            view.jobs.is_empty(),
            "an empty selection must not start a connection"
        );
        assert!(view
            .command("call server arbitrary-tool", manager.clone())
            .is_err());
        view.shutdown(manager).await.unwrap();
    }
}
