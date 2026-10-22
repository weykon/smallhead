#![cfg(feature = "test-server")]
use serde_json::json;
use smallhead_core::{tool::ToolImpl, ContentBlock, ToolContext};
use smallhead_mcp::*;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

fn config(mode: &str) -> ServerConfig {
    ServerConfig {
        command: Some(env!("CARGO_BIN_EXE_smallhead-mcp-test-server").into()),
        args: vec![mode.into()],
        connect_timeout_ms: 5000,
        list_timeout_ms: 2000,
        call_timeout_ms: 1000,
        ..Default::default()
    }
}
fn manager(config: ServerConfig) -> Arc<McpManager> {
    Arc::new(
        McpManager::new(McpConfig {
            mcp_servers: BTreeMap::from([("fixture".into(), config)]),
        })
        .unwrap(),
    )
}

#[tokio::test]
async fn sdk_stdio_roundtrip_structured_error_and_reconnect() {
    let manager = manager(config("stderr"));
    assert_eq!(
        manager.snapshots()[0].status,
        ConnectionStatus::Disconnected
    );
    assert!(matches!(
        manager.tools("fixture").await,
        Err(McpError::NotConnected)
    ));
    let ready = manager.connect("fixture").await.unwrap();
    assert_eq!(ready.status, ConnectionStatus::Ready);
    assert_eq!(ready.tool_count, 1);
    assert_eq!(manager.tools("fixture").await.unwrap()[0].name, "echo");
    let tool = McpTool::new(manager.clone());
    let result = tool.execute(json!({"action":"call","server":"fixture","tool":"echo","arguments":{"fail":true,"value":42}}), &ToolContext::default()).await.unwrap();
    assert!(result.is_error);
    let ContentBlock::Text { text } = &result.content[0] else {
        panic!("JSON text expected")
    };
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["structuredContent"]["value"], 42);
    assert_eq!(value["isError"], true);
    assert_eq!(
        manager.reconnect("fixture").await.unwrap().status,
        ConnectionStatus::Ready
    );
    manager.disconnect_all().await.unwrap();
    assert_eq!(
        manager.snapshots()[0].status,
        ConnectionStatus::Disconnected
    );
}

#[tokio::test]
async fn timeout_and_abort_fail_close_without_replay_and_keep_servers_independent() {
    for abort in [false, true] {
        let mut slow = config("");
        slow.call_timeout_ms = 100;
        let manager = Arc::new(
            McpManager::new(McpConfig {
                mcp_servers: BTreeMap::from([
                    ("fixture".into(), slow),
                    ("other".into(), config("")),
                ]),
            })
            .unwrap(),
        );
        manager.connect("fixture").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("submitted");
        let task = {
            let manager = manager.clone();
            let marker = marker.clone();
            tokio::spawn(async move {
                manager
                    .call("fixture", "echo", json!({"wait":true,"marker":marker}))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            manager.tools("fixture").await,
            Err(McpError::Busy)
        ));
        manager.connect("other").await.unwrap();
        if abort {
            task.abort();
            let _ = task.await;
        } else {
            assert!(matches!(
                task.await.unwrap(),
                Err(McpError::Timeout("call"))
            ));
        }
        assert_eq!(
            manager
                .snapshots()
                .iter()
                .find(|s| s.name == "fixture")
                .unwrap()
                .status,
            ConnectionStatus::Failed
        );
        assert!(matches!(
            manager.call("fixture", "echo", json!({})).await,
            Err(McpError::NotConnected)
        ));
        manager.disconnect_all().await.unwrap();
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "submitted once\n");
    }
}

#[tokio::test]
async fn initialize_and_discovery_failures_are_bounded() {
    for mode in ["hang-init", "slow-list", "loop-pages"] {
        let mut cfg = config(mode);
        if mode == "hang-init" {
            cfg.connect_timeout_ms = 100;
        }
        if mode == "slow-list" {
            cfg.list_timeout_ms = 100;
        }
        let manager = manager(cfg);
        assert!(manager.connect("fixture").await.is_err());
        assert_eq!(manager.snapshots()[0].status, ConnectionStatus::Failed);
        manager.disconnect_all().await.unwrap();
    }
}

#[tokio::test]
async fn protocol_errors_do_not_expose_remote_secrets_or_claim_no_execution() {
    let manager = manager(config(""));
    manager.connect("fixture").await.unwrap();
    let error = manager
        .call("fixture", "echo", json!({"protocol_error":true}))
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("fixture-secret"));
    assert!(error.contains("execution may have occurred"));
    manager.disconnect_all().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn disconnect_kills_owned_launcher_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("heartbeat");
    let mut cfg = config("group");
    cfg.args.push(marker.to_string_lossy().into_owned());
    let manager = manager(cfg);
    manager.connect("fixture").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !marker.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    manager.disconnect("fixture").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = std::fs::read_to_string(&marker).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(std::fs::read_to_string(marker).unwrap(), stopped);
}
