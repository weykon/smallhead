//! Offline example: two editable JS lenses around the normal ReAct loop.
use std::sync::Arc;

use smallhead_core::*;
use smallhead_lens::LensRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = LensRegistry::default();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lenses");
    registry.load(root.join("learning"))?;
    registry.load(root.join("evidence-check"))?;
    let session = registry.session();
    let provider = Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall {
            name: "echo".into(),
            input: serde_json::json!({"text":"source evidence"}),
        },
        MockResponse::AssistantText {
            text: "Candidate answer".into(),
        },
        MockResponse::AssistantText {
            text: "Reviewed answer based on the actual receipt".into(),
        },
    ]));
    let mut agent = AgentLoop::new(provider, vec![ToolKind::Echo(EchoTool)], "Offline demo");
    agent.set_policy_stack(session.policies());
    let result = agent
        .run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "Check a source, then explain it.".into(),
            }],
            timestamp: 0,
        })
        .await;
    session.shutdown().await?;
    result?;
    println!("{}", serde_json::to_string_pretty(&agent.state.messages)?);
    Ok(())
}
