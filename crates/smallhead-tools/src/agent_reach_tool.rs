//! `AgentReachTool` handler — exposes Agent-Reach CLI channels to the agent.
//!
//! This module is the provider-side implementation for
//! [`smallhead_core::AgentReachTool`]. It intentionally runs one-shot upstream
//! CLI commands rather than registering a long-lived service.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde_json::{json, Value};
use smallhead_core::{set_agent_reach_handler, ContentBlock, ToolContext, ToolError, ToolResult};

use crate::agent_reach::{
    fetch_channel, provider_for, upstream_available, AgentReachBiliProvider, AgentReachChannel,
    AgentReachSearchProvider, AgentReachYoutubeProvider, OutputKind, AGENT_REACH_PROVIDERS,
};
use crate::search_provider::SearchProvider;

/// Register the Agent-Reach handler into `smallhead_core`.
///
/// Call once during host startup before exposing `agent_reach` to the model.
pub fn register() {
    set_agent_reach_handler(handle_agent_reach_call);
}

fn handle_agent_reach_call(
    input: Value,
    _ctx: &ToolContext,
) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'static>> {
    Box::pin(async move { execute(input).await })
}

async fn execute(input: Value) -> Result<ToolResult, ToolError> {
    let action = match input.get("action").and_then(Value::as_str) {
        Some(action) => action,
        None => {
            return Ok(error_result(
                "Missing 'action'. Expected list, search, or fetch.",
            ))
        }
    };

    match action {
        "list" => Ok(success_result(list_channels())),
        "search" => search(input).await,
        "fetch" => fetch(input).await,
        other => Ok(error_result(format!(
            "Invalid action '{other}'. Expected list, search, or fetch."
        ))),
    }
}

fn list_channels() -> Value {
    let channels: Vec<Value> = AGENT_REACH_PROVIDERS
        .iter()
        .map(|provider| {
            json!({
                "channel": provider.channel.name(),
                "implemented": provider.channel.is_implemented(),
                "zero_config": provider.zero_config,
                "upstream_cli": provider.upstream_cli,
                "upstream_available": upstream_available(provider.upstream_cli),
                "search_upstream_cli": provider.search_upstream_cli(),
                "search_upstream_available": provider.search_args_prefix.is_some()
                    && upstream_available(provider.search_upstream_cli()),
                "supports_search": provider.search_args_prefix.is_some(),
                "supports_fetch": provider.fetch_args_prefix.is_some(),
                "output_kind": output_kind_label(provider.output_kind),
            })
        })
        .collect();
    json!({
        "success": true,
        "action": "list",
        "channels": channels,
        "search_supported_channels": ["search", "youtube", "bili"],
        "fetch_supported_channels": ["webpage", "rss", "youtube", "bili"],
        "install_hint": "agent-reach install --env=auto --system",
    })
}

async fn search(input: Value) -> Result<ToolResult, ToolError> {
    let channel = match parse_channel(&input) {
        Ok(channel) => channel,
        Err(result) => return Ok(result),
    };
    let query = match input.get("query").and_then(Value::as_str) {
        Some(query) if !query.trim().is_empty() => query.trim().to_string(),
        _ => return Ok(error_result("Missing or empty 'query' for search action.")),
    };
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, 30))
        .unwrap_or(8);
    let Some(provider) = search_provider_for(channel) else {
        return Ok(error_result(format!(
            "Channel '{}' does not support agent_reach search. Supported: search, youtube, bili.",
            channel.name()
        )));
    };

    let start = Instant::now();
    match provider.search(&query, limit).await {
        Ok(hits) => Ok(success_result(json!({
            "success": true,
            "action": "search",
            "channel": channel.name(),
            "provider_used": provider.name(),
            "query": query,
            "result_count": hits.len(),
            "duration_ms": start.elapsed().as_millis() as u64,
            "results": hits,
        }))),
        Err(error) => Ok(json_result(
            json!({
                "success": false,
                "action": "search",
                "channel": channel.name(),
                "query": query,
                "duration_ms": start.elapsed().as_millis() as u64,
                "error": error,
            }),
            true,
        )),
    }
}

async fn fetch(input: Value) -> Result<ToolResult, ToolError> {
    let channel = match parse_channel(&input) {
        Ok(channel) => channel,
        Err(result) => return Ok(result),
    };
    if !fetch_supported(channel) {
        return Ok(error_result(format!(
            "Channel '{}' does not support agent_reach fetch. Supported: webpage, rss, youtube, bili.",
            channel.name()
        )));
    }
    let target = match input.get("target").and_then(Value::as_str) {
        Some(target) if !target.trim().is_empty() => target.trim().to_string(),
        _ => return Ok(error_result("Missing or empty 'target' for fetch action.")),
    };
    let output_limit = input
        .get("output_limit_chars")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1024, 60_000))
        .unwrap_or(20_000);

    let start = Instant::now();
    match fetch_channel(channel, &target).await {
        Ok(output) => {
            let (text, truncated) = truncate_chars(&output, output_limit);
            let mut value = json!({
                "success": true,
                "action": "fetch",
                "channel": channel.name(),
                "target": target,
                "upstream_cli": provider_for(channel).upstream_cli,
                "duration_ms": start.elapsed().as_millis() as u64,
                "output_chars": output.chars().count(),
                "output_truncated": truncated,
                "output": text,
            });
            if matches!(
                channel,
                AgentReachChannel::Youtube | AgentReachChannel::Bili
            ) {
                value["content_kind"] = json!("video_metadata");
                value["transcript_included"] = json!(false);
            }
            Ok(success_result(value))
        }
        Err(error) => Ok(json_result(
            json!({
                "success": false,
                "action": "fetch",
                "channel": channel.name(),
                "target": target,
                "duration_ms": start.elapsed().as_millis() as u64,
                "error": error,
            }),
            true,
        )),
    }
}

fn parse_channel(input: &Value) -> Result<AgentReachChannel, ToolResult> {
    let Some(value) = input.get("channel").and_then(Value::as_str) else {
        return Err(error_result(
            "Missing 'channel'. Call action=list to inspect supported channels.",
        ));
    };
    AgentReachChannel::parse(value).ok_or_else(|| {
        error_result(format!(
            "Unknown channel '{value}'. Call action=list to inspect supported channels."
        ))
    })
}

fn search_provider_for(channel: AgentReachChannel) -> Option<&'static dyn SearchProvider> {
    match channel {
        AgentReachChannel::Search => Some(&AgentReachSearchProvider),
        AgentReachChannel::Youtube => Some(&AgentReachYoutubeProvider),
        AgentReachChannel::Bili => Some(&AgentReachBiliProvider),
        _ => None,
    }
}

fn fetch_supported(channel: AgentReachChannel) -> bool {
    matches!(
        channel,
        AgentReachChannel::Webpage
            | AgentReachChannel::Rss
            | AgentReachChannel::Youtube
            | AgentReachChannel::Bili
    )
}

fn output_kind_label(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Json => "json",
        OutputKind::Jsonl => "jsonl",
        OutputKind::Plain => "plain",
    }
}

fn truncate_chars(value: &str, limit: usize) -> (String, bool) {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= limit {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

fn success_result(value: Value) -> ToolResult {
    json_result(value, false)
}

fn error_result(msg: impl Into<String>) -> ToolResult {
    json_result(json!({"success": false, "error": msg.into()}), true)
}

fn json_result(value: Value, is_error: bool) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        is_error,
        content: vec![ContentBlock::Text {
            text: serde_json::to_string_pretty(&value).unwrap_or_default(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: &ToolResult) -> &str {
        match &result.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn list_returns_channel_catalog() {
        let result = execute(json!({"action": "list"})).await.unwrap();
        assert!(!result.is_error);
        let value: Value = serde_json::from_str(text(&result)).unwrap();
        assert!(value["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|channel| channel["channel"] == "youtube"));
        let bili = value["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["channel"] == "bili")
            .unwrap();
        assert_eq!(bili["upstream_cli"], "yt-dlp");
        assert_eq!(bili["search_upstream_cli"], "bili");
    }

    #[tokio::test]
    async fn missing_action_is_error() {
        let result = execute(json!({})).await.unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("Missing 'action'"));
    }

    #[tokio::test]
    async fn invalid_channel_is_error() {
        let result = execute(json!({"action": "search", "channel": "nope", "query": "rust"}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("Unknown channel"));
    }

    #[tokio::test]
    async fn unsupported_search_channel_is_error_without_shelling_out() {
        let result = execute(json!({"action": "search", "channel": "webpage", "query": "rust"}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(text(&result).contains("does not support"));
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        let (out, truncated) = truncate_chars("你好abc", 3);
        assert_eq!(out, "你好a");
        assert!(truncated);
    }
}
