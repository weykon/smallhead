//! 快速决策：后端保留原生语义，业务策略独立于响应解析。
mod laya;
mod protocol;
mod qwen;
#[cfg(test)]
mod tests;
mod tool;

pub use protocol::*;
pub use tool::FastDecisionTool;

use crate::tool::ToolError;
use serde_json::Value;

/// 可复用的本地服务客户端；地址只由应用配置。
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct DecisionClient {
    /// Laya 服务地址。
    pub laya_endpoint: String,
    /// Qwen 服务地址。
    pub qwen_endpoint: String,
    /// 单次请求超时，毫秒。
    pub timeout_ms: u64,
}
impl Default for DecisionClient {
    fn default() -> Self {
        Self {
            laya_endpoint: "http://127.0.0.1:3031".into(),
            qwen_endpoint: "http://127.0.0.1:3032".into(),
            timeout_ms: 5000,
        }
    }
}
impl DecisionClient {
    /// 使用应用环境变量构造，模型无法提供服务地址。
    pub fn from_env() -> Self {
        Self {
            laya_endpoint: std::env::var("SMALLHEAD_LAYA_URL")
                .unwrap_or_else(|_| Self::default().laya_endpoint),
            qwen_endpoint: std::env::var("SMALLHEAD_QWEN_URL")
                .unwrap_or_else(|_| Self::default().qwen_endpoint),
            ..Self::default()
        }
    }
    /// 一次调用 Laya 的多个类型问题，保留各字段输出。
    pub async fn evaluate(&self, request: &LayaRequest) -> Result<Value, ToolError> {
        laya::evaluate(self, request).await
    }
    /// 对 Qwen 候选评分，然后应用显式选择策略。
    pub async fn rerank(&self, request: &QwenRequest) -> Result<Value, ToolError> {
        qwen::rerank(self, request).await
    }
    async fn post(&self, endpoint: &str, path: &str, body: Value) -> Result<Value, ToolError> {
        if self.timeout_ms == 0 {
            return Err(invalid("timeout_ms 必须大于零"));
        }
        let encoded = serde_json::to_vec(&body).map_err(|e| invalid(e.to_string()))?;
        let limit = if path == "evaluate" { 32768 } else { 131072 };
        if encoded.len() > limit {
            return Err(invalid("请求过长，请显式缩小局部上下文"));
        }
        let started = std::time::Instant::now();
        crate::diagnostics::emit(
            "route",
            "decision_request",
            serde_json::json!({"path":path,"request":body}),
        );
        let result = async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_millis(self.timeout_ms))
                .build()
                .map_err(|e| transport_error(e, path))?;
            client
                .post(format!("{}/{path}", endpoint.trim_end_matches('/')))
                .json(&body)
                .send()
                .await
                .map_err(|e| transport_error(e, path))?
                .error_for_status()
                .map_err(|e| transport_error(e, path))?
                .json()
                .await
                .map_err(|e| transport_error(e, path))
        }
        .await;
        crate::diagnostics::emit(
            "route",
            "decision_response",
            serde_json::json!({"path":path,"elapsed_ms":started.elapsed().as_millis(),"response":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string)}),
        );
        result
    }
}
fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidInput(message.into())
}
fn service_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(format!("快速决策服务失败：{error}"))
}
fn transport_error(error: reqwest::Error, path: &str) -> ToolError {
    let hint = if error.is_connect() {
        let command = if path == "evaluate" {
            "sh experiments/laya/start.sh"
        } else {
            "sh experiments/qwen-reranker/start.sh"
        };
        format!("无法连接本地服务。demo/TUI 是客户端，不会自动启动模型服务；请另开终端运行 `{command}` 并保持运行，或检查配置的服务地址。")
    } else if error.is_timeout() {
        "本地模型请求超时；请检查服务是否仍在加载、忙碌或处理过长输入。".into()
    } else {
        "请检查模型服务返回的状态与日志。".into()
    };
    service_error(format!("{hint} 原因：{error}"))
}
fn probability(value: &Value) -> Result<f64, ToolError> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
        .ok_or_else(|| service_error("分数必须为 0..1 的有限数"))
}
fn identifier(value: &str, limit: usize) -> bool {
    let mut chars = value.chars();
    value.len() <= limit
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}
fn text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= limit
}
