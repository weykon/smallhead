//! 不需要模型 API 的进程预算与控制演示。
use smallhead_process::{OutputStream, ProcessLimits, ProcessManager, ProcessSpec};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::current_dir()?.join(".small-head/process-demo");
    let manager = ProcessManager::with_limits(
        root,
        ProcessLimits {
            max_concurrent: 1,
            max_stream_bytes: 1024,
            default_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    )?;
    println!("日志目录：{}", manager.log_root().display());
    println!("预算：最多同时运行 1 条命令，每条输出流最多保存 1024 字节。\n");

    println!("① 启动第一条命令，第二条命令排队");
    let first = manager
        .start(ProcessSpec::new("sleep 2; echo 第一条完成"))
        .await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.inspect(first).is_some_and(|r| r.pid.is_none()) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let queued = manager
        .start(ProcessSpec::new("echo 不应执行这一条"))
        .await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    println!(
        "第一条 {first}：{:?}",
        manager.inspect(first).map(|r| r.state)
    );
    println!(
        "第二条 {queued}：{:?}",
        manager.inspect(queued).map(|r| r.state)
    );

    println!("\n② 停止排队命令；不会产生 OS PID");
    manager.stop(queued).await?;
    let cancelled = manager.wait_for(queued, Duration::from_secs(2)).await?;
    println!("状态：{:?}，PID：{:?}", cancelled.state, cancelled.pid);
    let finished = manager.wait_for(first, Duration::from_secs(5)).await?;
    println!("第一条正常结束：{:?}", finished.state);

    println!("\n③ 无限输出触发预算停止，保留有限日志");
    let loud = manager
        .start(ProcessSpec::new(
            "while :; do echo 持续输出用于测试预算; done",
        ))
        .await?;
    let stopped = manager.wait_for(loud, Duration::from_secs(3)).await?;
    println!("执行 ID：{loud}，状态：{:?}", stopped.state);
    let page = manager
        .read_output(loud, OutputStream::Stdout, 0, 96)
        .await?;
    println!(
        "已保存 {} 字节，下一偏移 {}，任务完成={}，输出超限={}\n{}",
        page.stored_bytes, page.next_offset, page.complete, page.output_limited, page.text
    );

    println!("\n④ 查询历史执行，关闭管理器");
    for id in manager.list() {
        if let Some(record) = manager.inspect(id) {
            println!(
                "{} | {} | {}",
                id,
                record.state.as_label(),
                record.spec.command
            );
        }
    }
    manager.shutdown(Duration::from_secs(3)).await?;
    println!("已关闭。日志保留在上述目录；此 demo 不调用 Laya、Qwen 或大模型。");
    Ok(())
}
