//! 不依赖聊天模型 API 的快速决策体验；需先启动对应本地服务。
use serde_json::{json, Value};
use smallhead_core::{
    fast_decision::{DecisionClient, FastDecisionTool, LayaRequest, QwenRequest},
    fast_route::FastBackend,
    tool::{ToolContext, ToolImpl},
    ContentBlock,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "catalog".into());
    let input = match mode.as_str() {
        "catalog" => json!({"action":"discover"}),
        "laya" => {
            let request: LayaRequest = serde_json::from_value(
                json!({"state":{"query":"请用中文告诉我昨天订单的状态，不急"},"questions":{
                    "source":{"type":"choice","instructions":"选择最合适的数据来源","criteria":{"orders":"订单状态数据库","memory":"用户偏好记忆"}},
                    "priority":{"type":"score","instructions":"判断用户请求的紧急程度","criteria":["低","中","高"]},
                    "history":{"type":"noul","instructions":"用户是否询问过去的记录？"}
                }}),
            )?;
            json!({"action":"laya","request":request})
        }
        "qwen" => {
            let policy = match std::env::args().nth(2).as_deref().unwrap_or("rank_all") {
                "rank_all" => json!({"mode":"rank_all"}),
                "top_k" => json!({"mode":"top_k","k":2}),
                "filter" => json!({"mode":"filter","min_score":0.5}),
                "select_one" => json!({"mode":"select_one","min_score":0.5,"min_margin":0.1}),
                _ => anyhow::bail!("策略：rank_all | top_k | filter | select_one"),
            };
            let request: QwenRequest = serde_json::from_value(
                json!({"query":"用户希望我使用什么语言回复？","instruction":"按回答查询所需信息的相关程度排序记忆","candidates":[
                {"id":"memory.language","text":"用户希望用中文回复。"},
                {"id":"memory.project","text":"用户的项目使用 Rust。"},
                {"id":"memory.style","text":"用户希望解释简短清楚。"}
            ],"policy":policy}),
            )?;
            json!({"action":"qwen","request":request})
        }
        _ => anyhow::bail!(
            "用法：fast_decision_demo [catalog | laya | qwen [rank_all|top_k|filter|select_one]]"
        ),
    };
    println!("请求：\n{}", serde_json::to_string_pretty(&input)?);
    println!("结果仅为建议，未执行任何候选业务；示例阈值没有经过校准。\n");
    let tool = FastDecisionTool {
        client: DecisionClient::from_env(),
        allowed_backends: vec![FastBackend::Laya, FastBackend::Qwen],
    };
    let result = tool.execute(input, &ToolContext::default()).await?;
    if let ContentBlock::Text { text } = &result.content[0] {
        println!(
            "响应：\n{}",
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(text)?)?
        );
    }
    Ok(())
}
