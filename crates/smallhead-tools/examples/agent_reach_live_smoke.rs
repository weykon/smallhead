//! Live, read-only Agent-Reach smoke. Run one channel per invocation.
//! Video fetch returns metadata only; it never requests subtitle/media writes.
use serde_json::{json, Value};
use smallhead_core::{AgentReachTool, ContentBlock, ToolContext, ToolKind};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channel = std::env::args().nth(1).ok_or("expected channel or list")?;
    let target_override = std::env::args().nth(2);
    let input = match channel.as_str() {
        "list" => json!({"action":"list"}),
        "search" => {
            json!({"action":"search","channel":"search","query":"Rust programming language official website","limit":1})
        }
        "webpage" => {
            json!({"action":"fetch","channel":"webpage","target":target_override.as_deref().unwrap_or("https://example.com"),"output_limit_chars":1024})
        }
        "rss" => {
            json!({"action":"fetch","channel":"rss","target":target_override.as_deref().unwrap_or("https://hnrss.org/frontpage?count=1"),"output_limit_chars":2048})
        }
        "bili-search" | "youtube-search" => {
            let search_channel = channel.strip_suffix("-search").unwrap();
            json!({"action":"search","channel":search_channel,"query":"Rust","limit":1})
        }
        "youtube" | "bili" => {
            let target = if channel == "youtube" {
                "https://www.youtube.com/watch?v=jNQXAC9IVRw"
            } else {
                "https://www.bilibili.com/video/BV1xx411c7mD"
            };
            json!({"action":"fetch","channel":channel,"target":target,"output_limit_chars":1024})
        }
        _ => return Err("unknown smoke channel".into()),
    };
    smallhead_tools::agent_reach_tool::register();
    let tool = ToolKind::AgentReach(AgentReachTool);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        tool.execute("live-smoke", input, &ToolContext::default()),
    )
    .await??;
    println!("is_error={}", result.is_error);
    let failed = result.is_error;
    for block in result.content {
        if let ContentBlock::Text { text } = block {
            let value: Value = serde_json::from_str(&text)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
    }
    if failed {
        return Err("live smoke returned a tool error (see response above)".into());
    }
    Ok(())
}
