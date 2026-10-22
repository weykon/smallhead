use serde_json::json;
use smallhead_mcp::*;
use std::collections::BTreeMap;

fn parsed(value: serde_json::Value) -> Result<McpManager, McpError> {
    McpManager::new(serde_json::from_value(value).unwrap())
}

#[test]
fn configuration_is_lazy_and_rejects_ambiguous_or_unaddressable_servers() {
    let manager =
        parsed(json!({"mcpServers":{"safe-name":{"command":"not-an-installed-executable"}}}))
            .unwrap();
    assert_eq!(
        manager.snapshots()[0].status,
        ConnectionStatus::Disconnected
    );
    for value in [
        json!({"mcpServers":{"has space":{"command":"anything"}}}),
        json!({"mcpServers":{"one":{"command":"anything","url":"http://localhost"}}}),
        json!({"mcpServers":{"one":{"url":"http://user:secret@localhost"}}}),
        json!({"mcpServers":{"one":{"url":"http://localhost","http_headers":{"Authorization":"Bearer literal-secret"}}}}),
        json!({"mcpServers":{"one":{"command":"anything","call_timeout_ms":0}}}),
    ] {
        assert!(parsed(value).is_err());
    }
}

#[tokio::test]
async fn safe_errors_disabled_servers_and_reload_validation() {
    let manager = parsed(json!({"mcpServers":{
        "missing":{"command":"/nonexistent/secret-command"},
        "disabled":{"command":"/nonexistent/secret-command","disabled":true}
    }}))
    .unwrap();
    assert_eq!(
        manager.connect("missing").await.unwrap_err(),
        McpError::ExecutableNotFound
    );
    assert_eq!(
        manager.connect("disabled").await.unwrap_err(),
        McpError::Disabled
    );
    let snapshots = serde_json::to_string(&manager.snapshots()).unwrap();
    assert!(!snapshots.contains("secret-command"));
    assert!(!format!("{manager:?}").contains("secret-command"));
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mcp.json");
    std::fs::write(&file, "{broken secret").unwrap();
    assert!(manager.reload(&file).await.is_err());
    assert_eq!(manager.snapshots().len(), 2);
    std::fs::write(
        &file,
        serde_json::to_vec(&McpConfig {
            mcp_servers: BTreeMap::new(),
        })
        .unwrap(),
    )
    .unwrap();
    manager.reload(&file).await.unwrap();
    assert!(manager.snapshots().is_empty());
}

#[test]
fn load_is_bounded_and_does_not_leak_invalid_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{secret-text").unwrap();
    let error = McpManager::load(&path).unwrap_err().to_string();
    assert!(!error.contains("secret-text"));
    std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(matches!(McpManager::load(&path), Err(McpError::Config(_))));
}
