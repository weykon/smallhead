//! Loopback-only HTTP fixture. It implements the wire protocol directly so the
//! production client is exercised without adding another web framework.
use serde_json::{json, Value};
use smallhead_mcp::*;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

struct Fixture {
    url: String,
    calls: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    worker: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn fixture(fail_call: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let accepted = connections.clone();
    let worker = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let method = line.split_whitespace().next().unwrap_or("").to_owned();
            let mut length = 0;
            loop {
                line.clear();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            assert!(length < 1024 * 1024);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let rpc = request["method"].as_str().unwrap_or("");
            let (status, result) = match (method.as_str(), rpc) {
                ("GET", _) => (405, None),
                ("DELETE", _) => (200, None),
                ("POST", "initialize") => (
                    200,
                    Some(
                        json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"local-fixture","version":"1"}}),
                    ),
                ),
                ("POST", "tools/list") => (
                    200,
                    Some(json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})),
                ),
                ("POST", "tools/call") => {
                    count.fetch_add(1, Ordering::SeqCst);
                    if fail_call {
                        (404, None)
                    } else {
                        (
                            200,
                            Some(
                                json!({"content":[{"type":"text","text":"ok"}],"structuredContent":request["params"]["arguments"],"isError":false}),
                            ),
                        )
                    }
                }
                _ => (202, None),
            };
            let body = result
                .map(|r| json!({"jsonrpc":"2.0","id":request["id"],"result":r}).to_string())
                .unwrap_or_default();
            let response = format!("HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    Fixture {
        url,
        calls,
        connections,
        worker,
    }
}

fn manager(url: &str) -> McpManager {
    McpManager::new(McpConfig {
        mcp_servers: BTreeMap::from([(
            "http".into(),
            ServerConfig {
                url: Some(url.into()),
                connect_timeout_ms: 2000,
                list_timeout_ms: 2000,
                call_timeout_ms: 2000,
                ..Default::default()
            },
        )]),
    })
    .unwrap()
}

#[tokio::test]
async fn http_roundtrip_lazy_connect_and_client_only_disconnect() {
    let fixture = fixture(false).await;
    let manager = manager(&fixture.url);
    assert_eq!(
        manager.list().await[0].status,
        ConnectionStatus::Disconnected
    );
    assert_eq!(fixture.connections.load(Ordering::SeqCst), 0);
    assert_eq!(
        manager.connect("http").await.unwrap().status,
        ConnectionStatus::Ready
    );
    assert_eq!(manager.tools("http").await.unwrap().len(), 1);
    let result = manager
        .call("http", "echo", json!({"value":123}))
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["value"], 123);
    manager.disconnect("http").await.unwrap();
    assert!(
        !fixture.worker.is_finished(),
        "disconnect must not shut down external server"
    );
    manager.connect("http").await.unwrap();
    manager.disconnect_all().await.unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_http_call_is_never_reinitialized_or_replayed() {
    let fixture = fixture(true).await;
    let manager = manager(&fixture.url);
    manager.connect("http").await.unwrap();
    let error = manager.call("http", "echo", json!({})).await.unwrap_err();
    assert!(error.to_string().contains("execution may have occurred"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(manager.snapshots()[0].status, ConnectionStatus::Failed);
    manager.disconnect_all().await.unwrap();
}

#[tokio::test]
async fn missing_header_env_is_safe_and_does_not_connect() {
    let fixture = fixture(false).await;
    let manager = McpManager::new(McpConfig {
        mcp_servers: BTreeMap::from([(
            "http".into(),
            ServerConfig {
                url: Some(fixture.url.clone()),
                http_headers: BTreeMap::from([(
                    "Authorization".into(),
                    "SMALLHEAD_MCP_TEST_NONEXISTENT_HEADER_7CD25".into(),
                )]),
                ..Default::default()
            },
        )]),
    })
    .unwrap();
    assert!(matches!(
        manager.connect("http").await,
        Err(McpError::Config(_))
    ));
    assert_eq!(fixture.connections.load(Ordering::SeqCst), 0);
    assert!(!serde_json::to_string(&manager.snapshots())
        .unwrap()
        .contains("Authorization"));
}
