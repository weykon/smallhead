//! S1a demo — runs the 5 §9 scenarios through `ProcessManager` and prints the
//! state transitions, log directory layout, and final exit info.
//!
//! Run with: `cargo run -p smallhead-process --example s1_demo`
//!
//! Each scenario prints its ExecutionId, log directory, final state, exit
//! info, and a snippet of stdout. Exits 0 if all scenarios reach a terminal
//! state.
//!
//! Logs are written under a fresh `tempfile::TempDir` so the demo does not
//! pollute the user's home directory. Set `SMALLHEAD_DEMO_LOG_ROOT` to a
//! persistent path if you want to inspect the logs after the demo exits.

use smallhead_process::{
    ExecutionScope, ExitReason, ProcessManager, ProcessRecord, ProcessSpec, ProcessState,
};
use std::time::Duration;
use tempfile::TempDir;

/// Build a default `TaskBound` spec for the demo. `description` is set to the
/// command so the printed scenarios are self-describing.
fn spec(command: &str) -> ProcessSpec {
    let mut s = ProcessSpec::new(command);
    s.description = Some(command.into());
    s
}

/// `spec` plus an explicit deadline. Used by the timeout / cancel scenarios.
fn timed_spec(command: &str, ms: u64) -> ProcessSpec {
    spec(command).with_timeout(Duration::from_millis(ms))
}

/// One scenario row in the demo. `command` is the only field that varies —
/// every other knob (`scope`, `description`) is set by `spec()`.
struct Scenario {
    name: &'static str,
    spec: ProcessSpec,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Honour SMALLHEAD_DEMO_LOG_ROOT if the caller wants a persistent path,
    // otherwise drop everything under a fresh TempDir at the end.
    let dir = match std::env::var_os("SMALLHEAD_DEMO_LOG_ROOT") {
        Some(p) if !p.is_empty() => {
            let path = std::path::PathBuf::from(p);
            std::fs::create_dir_all(&path)?;
            // Hold a TempDir that points at an empty sibling so the `Drop`
            // doesn't actually delete the user-chosen root.
            TempDir::with_prefix_in(path.to_str().unwrap_or("."), path.clone())
                .unwrap_or_else(|_| TempDir::new().expect("tempdir"))
        }
        _ => TempDir::new()?,
    };
    let mgr = ProcessManager::with_log_root(dir.path().to_path_buf())?;

    let scenarios = [
        Scenario {
            name: "01 success: echo",
            spec: spec("echo s1a-demo-ok"),
        },
        Scenario {
            name: "02 non-zero exit",
            spec: spec("sh -c 'exit 3'"),
        },
        Scenario {
            name: "03 timeout: sleep",
            spec: timed_spec("sleep 30", 250),
        },
        Scenario {
            name: "04 stop: idempotent",
            spec: timed_spec("sleep 30", 30_000),
        },
        Scenario {
            name: "05 logs: two streams",
            spec: spec("echo on-stdout; echo on-stderr >&2"),
        },
    ];

    println!("=== smallhead-process S1a demo ===");
    println!("log_root = {}", dir.path().display());
    println!();

    let mut all_ok = true;
    for (i, sc) in scenarios.iter().enumerate() {
        println!("[{i:02}] {} — `{}`", sc.name, sc.spec.command);
        let id = mgr.start(sc.spec.clone()).await?;
        println!("     execution_id = {id}");
        println!(
            "     log_dir      = {}",
            dir.path().join(id.to_string()).display()
        );

        // Scenario 04 fires stop() before waiting, twice, to exercise the
        // idempotent contract.
        if i == 3 {
            mgr.stop(id).await?;
            mgr.stop(id).await?;
            println!("     stop() x 2 (idempotent)");
        }

        let rec = mgr.wait_for(id, Duration::from_secs(5)).await?;
        print_record(&rec);
        if let Some(snippet) =
            read_first_line(&dir.path().join(id.to_string()).join("stdout.log")).await
        {
            println!("     stdout_head  = {snippet:?}");
        }
        all_ok &= matches!(rec.state, ProcessState::Exited(_));
        println!();
    }

    // Show the manager log root to confirm it's a directory of execution ids.
    let mut entries: Vec<_> = std::fs::read_dir(dir.path())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    println!("log_root contains {} execution dirs", entries.len());

    if all_ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!("at least one scenario failed"))
    }
}

/// Pretty-print a terminal record. The format mirrors the column layout in the
/// CLI / TUI rows: `state`, `exit_code`, `signal`, `duration_ms`.
fn print_record(rec: &ProcessRecord) {
    match &rec.state {
        ProcessState::Exited(info) => {
            let reason = match info.reason {
                ExitReason::Natural => "Natural",
                ExitReason::Timeout => "Timeout",
                ExitReason::Cancelled => "Cancelled",
                ExitReason::SpawnError => "SpawnError",
                ExitReason::Lost => "Lost",
                ExitReason::OutputLimit => "OutputLimit",
            };
            println!("     state        = Exited({reason})");
            println!("     exit_code    = {:?}", info.exit_code);
            println!("     signal       = {:?}", info.signal);
            println!("     duration_ms  = {}", info.duration.as_millis());
        }
        ProcessState::Queued => {
            println!("     state        = Queued (should not happen after wait_for)")
        }
        ProcessState::Starting => println!("     state        = Starting"),
        ProcessState::Running => println!("     state        = Running"),
        ProcessState::Stopping => println!("     state        = Stopping"),
        ProcessState::Rejected(r) => println!("     state        = Rejected({r})"),
        ProcessState::SpawnFailed(r) => println!("     state        = SpawnFailed({r})"),
        ProcessState::Lost(r) => println!("     state        = Lost({r})"),
        ProcessState::CleanupFailed(r) => println!("     state        = CleanupFailed({r})"),
    }

    // A couple of header fields for context — pid + scope so the reader can
    // see the spec round-tripped.
    if let Some(pid) = rec.pid {
        println!("     pid          = {pid}");
    }
    println!(
        "     scope        = {}",
        match rec.spec.scope {
            ExecutionScope::TaskBound => "TaskBound",
            ExecutionScope::SessionService => "SessionService",
            ExecutionScope::PersistentService => "PersistentService",
        }
    );
}

/// Read the first non-empty line of `path`, trimmed. Returns `None` if the
/// file is missing or empty. Used for the stdout snippet line in the demo.
async fn read_first_line(path: &std::path::Path) -> Option<String> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}
