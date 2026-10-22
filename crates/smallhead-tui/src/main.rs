//! TUI 入口与终端生命周期。渲染、应用状态、Agent 和日志分模块维护。
mod app;
mod logs;
mod markdown;
mod mcp_services;
#[cfg(test)]
mod monad_live;
mod processes;
mod render;
mod runtime;
mod services;
mod spec_review;
mod task_plan;
mod turn_recovery;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::time::Duration;

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            terminal::LeaveAlternateScreen,
            event::DisableBracketedPaste,
            event::DisableMouseCapture,
            crossterm::cursor::Show
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(concat!(
            "smallhead TUI · Monad 聊天与诊断\n",
            "用法：cargo run -p smallhead-tui -- [--mock] [--prefer-fast=laya|qwen] [--resume ID] [--timeout-secs N]\n",
            "F1 / Ctrl+F1 聊天 · F2 日志 · Ctrl+P 进程 · Ctrl+S 服务 · F9 方案\n",
            "执行中 Enter 补充当前任务 · Ctrl+X 取消 · Ctrl+C 退出\n",
            "/plan 要求 先审阅方案 · /spec 路径 打开文档 · /retry 基于保留结果继续\n",
            "/mcp 查看连接 · /mcp connect NAME 连接 · 服务页 m 切换 MCP\n",
            "默认 /route auto；--prefer-fast=laya|qwen 可指定快速后端。\n",
            "--mock 使用固定计算脚本；默认连续120秒无模型输出或工具事件才超时，等待用户选择时暂停。"
        ));
        return Ok(());
    }
    let mut mock = false;
    let mut route = runtime::Route::Auto;
    let mut resume = None;
    let mut timeout = 120;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mock" => mock = true,
            "--prefer-fast=laya" => route = runtime::Route::Laya,
            "--prefer-fast=qwen" => route = runtime::Route::Qwen,
            "--resume" => {
                resume = Some(
                    iter.next()
                        .ok_or_else(|| anyhow::anyhow!("--resume 缺少ID"))?
                        .clone(),
                )
            }
            "--timeout-secs" => {
                timeout = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("缺少超时秒数"))?
                    .parse::<u64>()?;
                anyhow::ensure!(timeout > 0, "超时必须大于0");
            }
            _ => anyhow::bail!("未知参数：{arg}"),
        }
    }
    load_dotenv();
    // No stdout/stderr tracing subscriber while alternate screen is active.
    // Detailed model events are delivered through the scoped diagnostic channel.
    let mut app = app::App::new(
        runtime::Runtime::new(mock, timeout),
        route,
        smallhead_session::JsonFileStore::default_location().ok(),
    );
    if let Some(id) = resume {
        app.load(&id).await?;
    }
    terminal::enable_raw_mode()?;
    let guard = TerminalGuard;
    crossterm::execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        event::EnableBracketedPaste,
        event::EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let result = run(&mut terminal, &mut app).await;
    let cleanup = app.shutdown().await;
    drop(guard);
    cleanup.map_err(anyhow::Error::msg)?;
    result
}
async fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut app::App,
) -> Result<()> {
    while !app.should_quit {
        app.tick().await;
        terminal.draw(|f| render::draw(f, app))?;
        if event::poll(Duration::from_millis(25))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key).await,
                Event::Paste(text) => app.paste(&text),
                Event::Mouse(mouse) => app.on_mouse(mouse).await,
                _ => {}
            }
        }
    }
    Ok(())
}

fn load_dotenv() {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let path = d.join(".env");
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut loaded = 0usize;
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        let key = key.trim();
                        let mut value = value.trim().to_string();
                        if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                            value = value[1..value.len() - 1].to_string();
                        }
                        if std::env::var_os(key).is_none() {
                            std::env::set_var(key, value);
                            loaded += 1;
                        }
                    }
                }
                if loaded > 0 {
                    eprintln!("[dotenv] loaded {} var(s) from {}", loaded, path.display());
                }
                return;
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
}
