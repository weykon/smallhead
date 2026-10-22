use super::*;
use crate::{
    fast_route::FastBackend,
    message::ContentBlock,
    tool::{ToolContext, ToolImpl, ToolResult, ToolSpec},
};
use async_trait::async_trait;
use serde_json::json;

/// Agent 的快速决策入口；只返回建议，不执行候选代表的业务操作。
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct FastDecisionTool {
    /// 由应用配置的服务客户端。
    pub client: DecisionClient,
    /// 本轮用户偏好允许的后端；空列表表示只能发现能力。
    pub allowed_backends: Vec<FastBackend>,
}
impl FastDecisionTool {
    /// 能力目录是代码支持范围，不是服务健康探测。
    pub fn catalog(&self) -> Value {
        json!({"capabilities":[
            {"backend":"laya","allowed":self.allowed_backends.contains(&FastBackend::Laya),"action":"laya","question_types":["choice","score","noul"],"max_questions":16,"request_example":{"state":{"query":"这条请求需要查什么？"},"questions":{"source":{"type":"choice","instructions":"选择合适的数据来源","criteria":{"orders":"订单状态","memory":"用户偏好"}},"priority":{"type":"score","instructions":"判断请求紧急程度","criteria":["低","中","高"]},"needs_history":{"type":"noul","instructions":"用户是否明确要求历史记录？"}}},"semantics":"choice 单选分布；score 有序等级评分；noul 原生是非值。支持共享 state 的多问题请求。"},
            {"backend":"qwen","allowed":self.allowed_backends.contains(&FastBackend::Qwen),"action":"qwen","policies":["rank_all","top_k","filter","select_one"],"max_candidates":32,"request_example":{"query":"用户喜欢什么语言？","instruction":"按与查询的相关程度排序记忆","candidates":[{"id":"memory.a","text":"用户偏好中文"},{"id":"memory.b","text":"项目使用 Rust"}],"policy":{"mode":"top_k","k":1}},"semantics":"每个候选独立 yes/no 相关性分数；不要求跨候选求和为 1，不是业务正确率。"}
        ],"executed":false,"limits":"阈值需业务校准；不抽取任意业务参数，不证明权限、任务状态或执行成功；服务地址只由程序配置。"})
    }
}
#[async_trait]
impl ToolImpl for FastDecisionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name:"fast_decision".into(),label:"快速模型决策与排序".into(),
            description:"算术业务需要快速方法选择时，直接用 monad_bridge dispatch calculation 并填写允许的 model_choice=laya|qwen，bridge 内部完成快速选择与本地求值；无需重复调用本工具。本工具用于独立的候选决策、语义判断或排序。先 action=discover 查看能力、允许后端和请求例子。action=laya 用 request.state 与 request.questions 提交 choice/score/noul 多问题；action=qwen 用 request.query、instruction、candidates 和 policy 排序或筛选候选。候选用稳定 ID。结果是建议和原始分数，禁止声称已执行业务；失败时主模型继续处理，不得编造快速模型结果。Qwen 阈值策略只在调用方有明确阈值要求时使用，否则 rank_all。遵守本轮 allowed 后端，不能填写服务地址。".into(),
            input_schema:json!({"type":"object","properties":{
                "action":{"type":"string","enum":["discover","laya","qwen"]},
                "request":{"oneOf":[
                    {"type":"object","properties":{"state":{"type":"object"},"questions":{"type":"object","minProperties":1,"maxProperties":16,"additionalProperties":{"oneOf":[
                        {"type":"object","properties":{"type":{"const":"choice"},"instructions":{"type":"string"},"criteria":{"type":"object","minProperties":2,"maxProperties":32,"additionalProperties":{"type":"string"}}},"required":["type","instructions","criteria"],"additionalProperties":false},
                        {"type":"object","properties":{"type":{"const":"score"},"instructions":{"type":"string"},"criteria":{"type":"array","minItems":2,"maxItems":8,"items":{"type":"string"}}},"required":["type","instructions","criteria"],"additionalProperties":false},
                        {"type":"object","properties":{"type":{"const":"noul"},"instructions":{"type":"string"},"criteria":{"type":"object","properties":{"false":{"type":"string"},"true":{"type":"string"}},"required":["false","true"],"additionalProperties":false}},"required":["type","instructions"],"additionalProperties":false}
                    ]}}},"required":["state","questions"],"additionalProperties":false},
                    {"type":"object","properties":{"query":{"type":"string"},"instruction":{"type":"string"},"candidates":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"}},"required":["id","text"],"additionalProperties":false}},
                        "policy":{"oneOf":[
                            {"type":"object","properties":{"mode":{"const":"rank_all"}},"required":["mode"],"additionalProperties":false},
                            {"type":"object","properties":{"mode":{"const":"top_k"},"k":{"type":"integer","minimum":1},"min_score":{"type":"number","minimum":0,"maximum":1}},"required":["mode","k"],"additionalProperties":false},
                            {"type":"object","properties":{"mode":{"const":"filter"},"min_score":{"type":"number","minimum":0,"maximum":1}},"required":["mode","min_score"],"additionalProperties":false},
                            {"type":"object","properties":{"mode":{"const":"select_one"},"min_score":{"type":"number","minimum":0,"maximum":1},"min_margin":{"type":"number","minimum":0,"maximum":1}},"required":["mode","min_score","min_margin"],"additionalProperties":false}
                        ]}},"required":["query","instruction","candidates"],"additionalProperties":false}
                ]}
            },"required":["action"],"additionalProperties":false}),
        }
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        if ctx.cancel {
            return Err(service_error("请求已取消"));
        }
        let obj = input.as_object().ok_or_else(|| invalid("输入必须是对象"))?;
        if obj.keys().any(|k| k != "action" && k != "request") {
            return Err(invalid("未知字段，只接受 action 和 request"));
        }
        let output = match input["action"].as_str() {
            Some("discover") => self.catalog(),
            Some(action @ ("laya" | "qwen")) => {
                let backend = if action == "laya" {
                    FastBackend::Laya
                } else {
                    FastBackend::Qwen
                };
                if !self.allowed_backends.contains(&backend) {
                    return Err(invalid(
                        "本轮配置未允许该后端，请用 /route auto 或对应后端切换",
                    ));
                }
                if action == "laya" {
                    let request = serde_json::from_value(input["request"].clone())
                        .map_err(|e| invalid(e.to_string()))?;
                    crate::execution::ensure_fast_service(ctx, "laya", &self.client.laya_endpoint)
                        .await?;
                    self.client.evaluate(&request).await?
                } else {
                    let request = serde_json::from_value(input["request"].clone())
                        .map_err(|e| invalid(e.to_string()))?;
                    crate::execution::ensure_fast_service(ctx, "qwen", &self.client.qwen_endpoint)
                        .await?;
                    self.client.rerank(&request).await?
                }
            }
            _ => return Err(invalid("action 必须是 discover、laya 或 qwen")),
        };
        crate::diagnostics::emit("route", "decision_complete", output.clone());
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: vec![ContentBlock::Text {
                text: output.to_string(),
            }],
        })
    }
}
