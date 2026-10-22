//! smallhead-server CLI — run the local A2A bridge or send one message to a peer.

use clap::{Parser, Subcommand, ValueEnum};
use std::{path::PathBuf, sync::Arc};

use smallhead_server::a2a::{A2aClient, A2aMessage, A2aPermissions, A2aServer, A2aServerConfig};

#[derive(Parser)]
#[command(
    name = "smallhead-server",
    version,
    about = "Local A2A bridge + headless runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Access {
    Observe,
    Development,
}

#[derive(Subcommand)]
enum Command {
    /// Run the receive side: an axum A2A bridge on loopback.
    Serve {
        /// Port to bind (loopback only).
        #[arg(long, default_value_t = 3999)]
        port: u16,
        /// Agent name advertised in the agent card and accepted as `to`.
        #[arg(long, default_value = "smallhead")]
        name: String,
        /// Local capability ceiling. Requests must explicitly opt into writes/execution.
        #[arg(long, value_enum, default_value = "observe")]
        access: Access,
        /// Working directory for file tools and commands (not a filesystem sandbox).
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Separate A2A process/service state; defaults to ~/.smallhead/a2a/runs.
        #[arg(long)]
        process_root: Option<PathBuf>,
        /// Override the conversation snapshot directory.
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        /// Optional per-task model/tool/Lens diagnostic JSONL directory.
        #[arg(long)]
        diagnostics_dir: Option<PathBuf>,
    },
    /// Send one message to a peer and print its task reply.
    Send {
        /// Peer base URL, e.g. http://127.0.0.1:3999
        peer: String,
        /// Message text.
        text: String,
        #[arg(long, default_value = "research")]
        intent: String,
        /// Request write and execution capabilities from a development host.
        #[arg(long)]
        development: bool,
        /// Continue a previous A2A conversation.
        #[arg(long)]
        conversation: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            port,
            name,
            access,
            cwd,
            process_root,
            sessions_dir,
            diagnostics_dir,
        } => {
            let runtime = Arc::new(
                smallhead_server::driver::HeadlessRuntime::from_env(
                    cwd,
                    matches!(access, Access::Development),
                    process_root,
                )
                .map_err(anyhow::Error::msg)?,
            );
            A2aServer::new(A2aServerConfig {
                name,
                port,
                sessions_dir: sessions_dir.or_else(A2aServerConfig::default_sessions_dir),
            })
            .with_runtime(runtime)
            .with_diagnostics_dir(diagnostics_dir)
            .run_until(shutdown_signal())
            .await
            .map_err(anyhow::Error::msg)?;
        }
        Command::Send {
            peer,
            text,
            intent,
            development,
            conversation,
        } => {
            let client = A2aClient::new(peer);
            let reply = client
                .send(&A2aMessage {
                    message_id: None,
                    conversation_id: conversation,
                    from: "smallhead".into(),
                    to: Some("*".into()),
                    kind: "request".into(),
                    intent: Some(intent),
                    text,
                    reply_to: None,
                    permissions: A2aPermissions {
                        can_mutate: development,
                        can_execute: development,
                        ..A2aPermissions::default()
                    },
                    refs: vec![],
                    ttl: 3,
                })
                .await
                .map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&reply)?);
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
