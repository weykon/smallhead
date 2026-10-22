//! Opt-in real-provider exercise of the actual TUI runtime and steering inbox.

use std::{fs::File, io::Write, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use smallhead_core::{
    monad::RunState, AgentMessage, ContentBlock, SpecBroker, SteeringInbox, TaskId,
};
use tokio::sync::{mpsc, oneshot};

use crate::runtime::{Route, Runtime, SpawnRequest};

const INITIAL: &str = "帮我看看 smallhead 添加和管理 MCP 服务的体验。启动两个独立的研究 Monad，任务分别叫“服务研究”和“界面研究”：服务研究查启动、连接、退出的真实行为，界面研究查 TUI 用户能看到和操作什么。两个任务都先报告一次进展，再读当前代码，关键发现及时用 finding 汇报。主任务留在协调位置，后面我会补充要求。最后综合成一个一致的改进方案，写出代码证据、矛盾和限制，先别改代码，也不要启动或停止真实服务，不运行 shell。请控制调查范围，只读取必要文件。仓库入口文件：crates/smallhead-process/src/service.rs、crates/smallhead-mcp/src/manager.rs、crates/smallhead-mcp/src/config.rs、crates/smallhead-tui/src/mcp_services.rs、crates/smallhead-tui/src/services.rs 和 crates/smallhead-tui/src/app.rs。把这些路径放进委派上下文，只需本地代码证据，不需要网络搜索。";
const CHANGE: &str = "补充一下，我特别关心外面已经启动的服务：用户只是连接进来，退出 smallhead 不能关闭它。先暂停“界面研究”那个 Monad，等到它真的确认暂停；把这个新要求同步给“服务研究”，让它优先查清程序凭什么区分自己启动的服务和外部服务，特别检查 MCP 和普通 ServiceManager 是否真的用同一种归属机制。让它汇报真实依据，不要猜。界面研究先保持暂停，等我说继续。";
const RESUME: &str = "现在继续“界面研究”。请先把服务研究实际查到的事实和代码依据传给它，然后恢复，让它按这些事实修正界面建议。最终别拼接两份报告，要给一致的方案，说明我的补充改变了哪些判断，哪些只是建议、哪些是已验证事实。无需等我再确认，也不需要发布 spec 审批，只给研究结论。";
const FOCUSED: &str = "帮我研究一个很小的改进：MCP 服务面板能否让用户明白退出 smallhead 时会发生什么？启动两个独立 Monad，名字叫“服务研究”和“界面研究”。服务研究只查 crates/smallhead-process/src/service.rs 与 crates/smallhead-mcp/src/manager.rs，关注外部服务和本程序创建的资源，其他生命周期细节不展开；界面研究只读 crates/smallhead-tui/src/mcp_services.rs，关注现在怎么显示 stdio/HTTP、连接状态、退出与断开，不读 app.rs 或其他大文件。两者都先报 progress，关键发现用 finding；找不到直接证据就明确说没有，不做网络搜索、不读测试代码、不改文件、不运行命令或连接真实服务。每个报告控制在 500 中文字以内。主 Monad 负责协调，稍后我会追加要求；最后给一份简短且一致的结论。把上述路径和范围准确写入各自的委派上下文，主任务不需要自己再读一遍所有文件。";

fn send(steering: &SteeringInbox, transcript: &mut File, text: &str, stage: &str) -> Result<()> {
    let id = steering.try_push(
        vec![ContentBlock::Text { text: text.into() }],
        smallhead_session::now_millis(),
    )?;
    writeln!(
        transcript,
        "{}",
        json!({"stage":stage,"steering_id":id,"text":text})
    )?;
    transcript.flush()?;
    eprintln!("live scenario: injected {stage} (steering {id})");
    Ok(())
}

fn pause_acknowledged(event: &smallhead_core::diagnostics::DiagnosticEvent, ui: &TaskId) -> bool {
    if event.layer != "request" || event.event != "provider_request" {
        return false;
    }
    event.data["request"]["messages"]
        .as_array()
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["text"].as_str().is_some_and(|text| {
                            text.lines().any(|line| {
                                serde_json::from_str::<Value>(line).is_ok_and(|payload| {
                                    payload["childtask"] == ui.0 && payload["kind"] == "paused"
                                })
                            })
                        })
                    })
                })
            })
        })
}

#[tokio::test]
#[ignore = "uses the configured real model; run explicitly with --ignored --nocapture"]
async fn monad_live_collaboration() -> Result<()> {
    // Cargo runs binary tests from the package directory; TUI runs from the
    // user's working directory. This opt-in test runs alone from the workspace.
    std::env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))?;
    crate::load_dotenv();
    ensure!(
        !std::env::var("MINIMAX_API_KEY")
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "live test requires MINIMAX_API_KEY; mock is forbidden"
    );
    let output = PathBuf::from(
        std::env::var_os("SMALLHEAD_MONAD_LIVE_OUTPUT")
            .context("set SMALLHEAD_MONAD_LIVE_OUTPUT to a fresh output directory")?,
    );
    std::fs::create_dir(&output).context("output directory must be new")?;
    let mut events = File::create(output.join("events.jsonl"))?;
    let mut inputs = File::create(output.join("inputs.jsonl"))?;
    let initial = if std::env::var_os("SMALLHEAD_MONAD_LIVE_FOCUSED").is_some() {
        FOCUSED
    } else {
        INITIAL
    };
    writeln!(inputs, "{}", json!({"stage":"initial","text":initial}))?;
    let runtime = Runtime::new(false, 120);
    ensure!(!runtime.mock, "real provider required");
    let task_id = TaskId::new();
    let steering = SteeringInbox::default();
    let (sink, mut diagnostics) = mpsc::unbounded_channel();
    let (cancel_tx, cancel) = oneshot::channel();
    let mut cancel_tx = Some(cancel_tx);
    let mut worker = runtime.spawn_controlled(
        SpawnRequest {
            messages: vec![],
            text: initial.into(),
            task_id: task_id.clone(),
            route: Route::Auto,
            sink,
            cancel,
            artifact_root: Some(output.join("artifacts")),
        },
        steering.clone(),
        Arc::new(SpecBroker::new()),
    );
    let deadline = tokio::time::sleep(Duration::from_secs(420));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut change_sent = false;
    let mut resume_sent = false;
    let mut pause_ack = false;
    let mut paused_task: Option<TaskId> = None;
    let mut last_states = String::new();
    let finished = loop {
        tokio::select! {
            result = &mut worker => break Some(result.context("runtime worker panicked")?),
            _ = &mut deadline => {
                if let Some(cancel) = cancel_tx.take() { let _ = cancel.send(()); }
                let settled = tokio::time::timeout(Duration::from_secs(20), &mut worker).await;
                match settled {
                    Ok(result) => break Some(result.context("cancelled worker panicked")?),
                    Err(_) => { runtime.monads.cancel_all(); worker.abort(); let _ = worker.await; break None; }
                }
            }
            Some(event) = diagnostics.recv() => {
                if let Some(ui) = &paused_task { pause_ack |= pause_acknowledged(&event, ui); }
                if event.layer == "tool" && event.event == "execute_start" {
                    eprintln!("parent tool: {} {}", event.data["name"], event.data["input"]["action"]);
                    if event.data["name"] == "monad_delegate" && event.data["input"]["action"] == "pause" {
                        paused_task = event.data["input"]["task_id"].as_str().map(|id| TaskId(id.into()));
                    }
                }
                writeln!(events, "{}", serde_json::to_string(&event)?)?;
                events.flush()?;
            }
            _ = tick.tick() => {
                let runs: Vec<_> = runtime.monads.snapshot().into_iter().filter(|run|
                    run.handoff.parent_task_id.as_ref() == Some(&task_id)).collect();
                let states = runs.iter().map(|run| format!("{}={:?}",run.handoff.objective,run.state)).collect::<Vec<_>>().join("; ");
                if states != last_states { eprintln!("children: {states}"); last_states = states; }
                if output.join("cancel").exists() || !runtime.choices.pending().is_empty()
                    || runs.iter().any(|run| run.state.is_terminal() && run.state != RunState::Completed) {
                    if let Some(cancel) = cancel_tx.take() {
                        eprintln!("live scenario: stopping on cancellation, unexpected choice, or failed child");
                        let _ = cancel.send(());
                    }
                }
                if !change_sent && runs.len() >= 2 {
                    send(&steering, &mut inputs, CHANGE, "change_and_pause")?;
                    change_sent = true;
                }
                if change_sent && !resume_sent && pause_ack && runs.iter().any(|run|
                    Some(&run.handoff.task_id) != paused_task.as_ref()
                    && run.state == RunState::Completed) {
                    send(&steering, &mut inputs, RESUME, "resume_and_merge")?;
                    resume_sent = true;
                }
            }
        }
    };
    while let Ok(event) = diagnostics.try_recv() {
        writeln!(events, "{}", serde_json::to_string(&event)?)?;
    }
    events.flush()?;
    runtime.monads.cancel_all();
    tokio::time::timeout(Duration::from_secs(20), runtime.monads.wait_idle()).await?;
    let runs = runtime.monads.snapshot();
    let saved_runs: Vec<_> = runs
        .iter()
        .map(|run| {
            json!({
                "run_id":run.run_id,"handoff":run.handoff,"state":format!("{:?}",run.state),
                "cleanup":format!("{:?}",run.cleanup),"result":run.result,
                "error":run.error.as_ref().map(ToString::to_string),
            })
        })
        .collect();
    std::fs::write(
        output.join("runs.json"),
        serde_json::to_vec_pretty(&saved_runs)?,
    )?;
    let mut completed = false;
    if let Some(finished) = finished {
        std::fs::write(
            output.join("messages.json"),
            serde_json::to_vec_pretty(&finished.messages)?,
        )?;
        let final_text = finished
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Assistant {
                    content,
                    tool_calls,
                    ..
                } if tool_calls.is_empty() => Some(
                    content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_default();
        std::fs::write(output.join("answer.md"), final_text)?;
        completed = matches!(
            finished.result,
            Ok(smallhead_core::MonadResult::Completed { .. })
        );
        std::fs::write(
            output.join("outcome.json"),
            serde_json::to_vec_pretty(&json!({
                "result":finished.result,"change_sent":change_sent,"pause_acknowledged":pause_ack,
                "resume_sent":resume_sent,"runtime_parent":task_id,"model":runtime.label,
            }))?,
        )?;
    }
    eprintln!("live evidence: {}", output.display());
    ensure!(
        completed
            && change_sent
            && pause_ack
            && resume_sent
            && runs.len() >= 2
            && runs.iter().all(|run| run.state == RunState::Completed),
        "live collaboration did not satisfy all phases; inspect saved evidence"
    );
    Ok(())
}
