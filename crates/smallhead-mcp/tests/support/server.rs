//! Hermetic SDK server used only by `cargo test --features test-server`.
use rmcp::{model::*, service::RequestContext, ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::json;

struct Fixture {
    mode: String,
}
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if self.mode == "slow-list" {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        let mut result: ListToolsResult = serde_json::from_value(json!({"tools":[{
            "name":"echo", "description":"Untrusted fixture description", "inputSchema":{"type":"object"}
        }]})).unwrap();
        if self.mode == "loop-pages" {
            result.next_cursor = Some("repeat".into());
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        if let Some(path) = args.get("marker").and_then(|v| v.as_str()) {
            use std::io::Write;
            let mut marker = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(marker, "submitted once").unwrap();
        }
        if args.get("wait").and_then(|v| v.as_bool()) == Some(true) {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        if args.get("protocol_error").and_then(|v| v.as_bool()) == Some(true) {
            return Err(ErrorData::internal_error(
                "fixture-secret-must-not-leak",
                None,
            ));
        }
        let failed = args.get("fail").and_then(|v| v.as_bool()).unwrap_or(false);
        let result = if failed {
            CallToolResult::structured_error(json!(args))
        } else {
            CallToolResult::structured(json!(args))
        };
        Ok(CallToolResponse::Complete(result))
    }
}

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "heartbeat" {
        let path = std::env::args().nth(2).unwrap();
        let mut count = 0u64;
        loop {
            std::fs::write(&path, count.to_string()).unwrap();
            count += 1;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    if mode == "hang-init" {
        std::future::pending::<()>().await;
    }
    if mode == "group" {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("heartbeat")
            .arg(std::env::args().nth(2).unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Keep the launcher alive after EOF so SDK forced shutdown kills its group.
        let service = Fixture {
            mode: String::new(),
        }
        .serve(rmcp::transport::stdio())
        .await
        .unwrap();
        let _ = service.waiting().await;
        let _owned_child = child;
        std::future::pending::<()>().await;
    }
    if mode == "stderr" {
        use std::io::Write;
        std::io::stderr()
            .write_all(&vec![b'x'; 512 * 1024])
            .unwrap();
    }
    let service = Fixture { mode }
        .serve(rmcp::transport::stdio())
        .await
        .unwrap();
    let _ = service.waiting().await;
}
