//! `WebSearchTool` — 统一多源 web 搜索,Phase 2 入口。
//!
//! LLM 一次 call 就可以拣 `kind` 走对应 chain:
//! - `web`  → Baidu/Bing.cn/Mojeek/DDG (CJK) 或 Bing.com/Mojeek/DDG (en)
//! - `wiki` → Wikipedia (按 cjk 拣 zh/en)
//! - `code` → GitHub repos
//! - `news` → Hacker News stories
//!
//! 所有 chain 由 `search_provider::run_fallback_chain` 跑,
//! 自动接 `provider_health::global()` 做 skip 过滤 + 失败记录。
//!
//! ## 注册
//!
//! Host crate 启动时调一次 [`register`],把 handler 注入到
//! `smallhead_core::set_web_search_handler`。之后
//! `ToolKind::WebSearch(WebSearchTool)` 即可直接 dispatch。
//!
//! ## 输出
//!
//! Output envelope 故意用 `results` 平铺(唔用 `data.web` / `data.wiki` 之类),
//! 方便 LLM 一眼睇出呢个系 unified 结果。`provider_used` 字段会讲明
//! 边个 source 真正 hit。
//!
//! ```json
//! {
//!   "success": true,
//!   "query": "rust async runtime",
//!   "kind": "web",
//!   "provider_used": "mojeek",
//!   "result_count": 5,
//!   "results": [
//!     { "kind": "web", "title": "...", "url": "...", "description": "...", "position": 1 },
//!     ...
//!   ]
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde::Deserialize;
use smallhead_core::{set_web_search_handler, ContentBlock, ToolContext, ToolError, ToolResult};

use crate::github::GithubProvider;
use crate::hackernews::HackerNewsProvider;
use crate::search_provider::{run_fallback_chain, SearchProvider, SourceKind};
use crate::web_search::{
    BaiduProvider, BingCnProvider, BingComProvider, DuckDuckGoProvider, MojeekProvider,
};
use crate::wiki::WikipediaProvider;

/// LLM 传嘅 `kind` 字段(序列化时细写,解析时宽)。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    /// General web search.
    Web,
    /// Wikipedia search.
    Wiki,
    /// Source-code and repository search.
    Code,
    /// News and discussion search.
    News,
}

impl SearchKind {
    fn to_source_kind(self) -> SourceKind {
        match self {
            SearchKind::Web => SourceKind::Web,
            SearchKind::Wiki => SourceKind::Wiki,
            SearchKind::Code => SourceKind::Code,
            SearchKind::News => SourceKind::News,
        }
    }

    fn as_label(self) -> &'static str {
        match self {
            SearchKind::Web => "web",
            SearchKind::Wiki => "wiki",
            SearchKind::Code => "code",
            SearchKind::News => "news",
        }
    }
}

/// [`chain_for_kind`] 嘅 smallhead-tools 端实现 —— 所有 provider 集中喺呢度。
///
/// CJK 决定 web chain 用 zh 系(Baidu + cn.bing)定 en 系(bing.com),
/// 其他 kind 唔受 cjk 影响(wiki 嘅 cjk 喺调用方按 query 内容拣 lang)。
pub fn chain_for_kind(kind: SourceKind, cjk: bool) -> Vec<&'static dyn SearchProvider> {
    match kind {
        SourceKind::Web => {
            if cjk {
                vec![
                    &BaiduProvider,
                    &BingCnProvider,
                    &MojeekProvider,
                    &DuckDuckGoProvider,
                ]
            } else {
                vec![&BingComProvider, &MojeekProvider, &DuckDuckGoProvider]
            }
        }
        SourceKind::Wiki => {
            // 按 query cjk 拣 zh/en wiki,各自走单源(冇 fallback chain)
            if cjk {
                vec![WikipediaProvider::chinese()]
            } else {
                vec![WikipediaProvider::english()]
            }
        }
        SourceKind::Code => vec![&GithubProvider],
        SourceKind::News => vec![&HackerNewsProvider],
    }
}

/// 注册统一 web 搜索 handler 到 `smallhead_core`。
///
/// 调用多次会 panic(OnceLock)。Host crate 启动期调一次即可。
pub fn register() {
    set_web_search_handler(handle_web_search_call);
}

fn handle_web_search_call(
    input: serde_json::Value,
    _ctx: &ToolContext,
) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'static>> {
    Box::pin(async move { execute(input).await })
}

async fn execute(input: serde_json::Value) -> Result<ToolResult, ToolError> {
    let query = match input.get("query").and_then(|v| v.as_str()) {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => {
            return Ok(error_result("Missing or empty 'query' field"));
        }
    };

    let kind: SearchKind = match input.get("kind") {
        Some(v) => match serde_json::from_value::<SearchKind>(v.clone()) {
            Ok(k) => k,
            Err(e) => {
                return Ok(error_result(&format!(
                    "Invalid 'kind' field: {e}. Expected one of: web, wiki, code, news."
                )));
            }
        },
        None => {
            return Ok(error_result(
                "Missing 'kind' field. Expected one of: web, wiki, code, news.",
            ));
        }
    };

    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, 30))
        .unwrap_or(8);

    let source_kind = kind.to_source_kind();
    let cjk = crate::search_provider::contains_cjk(&query);
    let chain = chain_for_kind(source_kind, cjk);

    let start = Instant::now();
    let outcome = run_fallback_chain(&chain, &query, limit).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match outcome {
        Ok((provider_used, hits)) => {
            let body = serde_json::json!({
                "success": true,
                "query": query,
                "kind": kind.as_label(),
                "provider_used": provider_used,
                "result_count": hits.len(),
                "duration_ms": duration_ms,
                "results": hits,
            });
            Ok(ToolResult {
                tool_call_id: String::new(),
                is_error: false,
                content: vec![ContentBlock::Text {
                    text: serde_json::to_string_pretty(&body).unwrap_or_default(),
                }],
            })
        }
        Err(e) => Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: true,
            content: vec![ContentBlock::Text {
                text: format!("search failed: {e}"),
            }],
        }),
    }
}

fn error_result(msg: &str) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        is_error: true,
        content: vec![ContentBlock::Text { text: msg.into() }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kind_field() {
        let v: serde_json::Value = serde_json::json!("web");
        let k: SearchKind = serde_json::from_value(v).unwrap();
        assert!(matches!(k, SearchKind::Web));

        let v: serde_json::Value = serde_json::json!("wiki");
        let k: SearchKind = serde_json::from_value(v).unwrap();
        assert!(matches!(k, SearchKind::Wiki));

        let v: serde_json::Value = serde_json::json!("code");
        let k: SearchKind = serde_json::from_value(v).unwrap();
        assert!(matches!(k, SearchKind::Code));

        let v: serde_json::Value = serde_json::json!("news");
        let k: SearchKind = serde_json::from_value(v).unwrap();
        assert!(matches!(k, SearchKind::News));
    }

    #[test]
    fn rejects_unknown_kind() {
        let v: serde_json::Value = serde_json::json!("garbage");
        let r: Result<SearchKind, _> = serde_json::from_value(v);
        assert!(r.is_err());
    }

    #[test]
    fn cjk_routing_for_web() {
        let chain_zh = chain_for_kind(SourceKind::Web, true);
        let chain_en = chain_for_kind(SourceKind::Web, false);
        assert!(chain_zh.iter().any(|p| p.name() == "baidu"));
        assert!(chain_zh.iter().any(|p| p.name() == "bing-cn"));
        assert!(chain_en.iter().any(|p| p.name() == "bing"));
        assert!(!chain_en.iter().any(|p| p.name() == "baidu"));
        // Mojeek + DDG 喺两条链都出现(共通 fallback)
        assert!(chain_zh.iter().any(|p| p.name() == "mojeek"));
        assert!(chain_zh.iter().any(|p| p.name() == "duckduckgo"));
    }

    #[test]
    fn cjk_routing_for_wiki() {
        let chain_zh = chain_for_kind(SourceKind::Wiki, true);
        let chain_en = chain_for_kind(SourceKind::Wiki, false);
        assert_eq!(chain_zh.len(), 1);
        assert_eq!(chain_en.len(), 1);
        // 同一个 wikipedia name,但 lang 唔同
        assert_eq!(chain_zh[0].name(), "wikipedia");
        assert_eq!(chain_en[0].name(), "wikipedia");
        // Wiki 嗰两个 static instance 唔同
        assert!(!std::ptr::eq(chain_zh[0], chain_en[0]));
    }

    #[test]
    fn code_chain_is_github_only() {
        let chain = chain_for_kind(SourceKind::Code, false);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "github");
    }

    #[test]
    fn news_chain_is_hn_only() {
        let chain = chain_for_kind(SourceKind::News, false);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "hackernews");
    }

    #[test]
    fn source_kind_label() {
        assert_eq!(SearchKind::Web.as_label(), "web");
        assert_eq!(SearchKind::Wiki.as_label(), "wiki");
        assert_eq!(SearchKind::Code.as_label(), "code");
        assert_eq!(SearchKind::News.as_label(), "news");
    }

    #[tokio::test]
    async fn execute_rejects_missing_query() {
        let input = serde_json::json!({"kind": "web"});
        let result = execute(input).await.unwrap();
        assert!(result.is_error);
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("Missing or empty 'query' field"));
    }

    #[tokio::test]
    async fn execute_rejects_empty_query() {
        let input = serde_json::json!({"query": "   ", "kind": "web"});
        let result = execute(input).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn execute_rejects_invalid_kind() {
        let input = serde_json::json!({"query": "x", "kind": "garbage"});
        let result = execute(input).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn execute_rejects_missing_kind() {
        let input = serde_json::json!({"query": "x"});
        let result = execute(input).await.unwrap();
        assert!(result.is_error);
    }
}
