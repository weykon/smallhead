//! `HackerNewsProvider` — Hacker News via Algolia 公开 API (无 auth, 干净 JSON)。
//!
//! Endpoint: `https://hn.algolia.com/api/v1/search?query=...&tags=story&hitsPerPage={limit}`
//! - 返回: hits[] with { title, url/objectID, author, points, num_comments, created_at }
//! - 全部 JSON,无 anti-bot,无 captcha
//! - 限速宽松(每秒几 req)
//!
//! Phase 1: `HackerNewsProvider` 实现 `SearchProvider` trait,统一返 `SearchHit::Story` 变体。
//! 旧 `StoryHit` struct 被砍,信封装喺 `SearchHit` enum(`kind: "story"` tag)。
//!
//! Phase 3 之后唔再有 `hackernews_search` tool —— LLM 改用 unified `search` tool 加 `kind=news`。
//! HN 系 News 链嘅唯一 source,无需 fallback。

use async_trait::async_trait;
use serde::Deserialize;

use crate::http_client::build_client;
use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

const HN_ENDPOINT: &str = "https://hn.algolia.com/api/v1/search";

/// Hacker News story search provider backed by Algolia.
pub struct HackerNewsProvider;

#[async_trait]
impl SearchProvider for HackerNewsProvider {
    fn name(&self) -> &'static str {
        "hackernews"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::News
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_hn(query, limit as u32).await
    }
}

#[derive(Debug, Deserialize)]
struct HnResponse {
    hits: Vec<HnHit>,
}

#[derive(Debug, Deserialize)]
struct HnHit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    points: Option<u64>,
    #[serde(default)]
    num_comments: Option<u64>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    object_id: Option<String>,
}

async fn search_hn(query: &str, limit: u32) -> Result<Vec<SearchHit>, String> {
    let client = build_client("smallhead/0.1")?;

    let body = client
        .get(HN_ENDPOINT)
        .header("Accept", "application/json")
        .query(&[
            ("query", query),
            ("tags", "story"),
            ("hitsPerPage", &limit.to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    let parsed: HnResponse = serde_json::from_str(&body).map_err(|e| {
        format!(
            "parse json: {e}; body[:300]={}",
            &body[..body.len().min(300)]
        )
    })?;

    Ok(parsed
        .hits
        .into_iter()
        .filter(|h| h.title.is_some()) // skip jobs/events without title
        .enumerate()
        .map(|(idx, h)| SearchHit::Story {
            title: h.title.unwrap_or_default(),
            url: h.url.unwrap_or_else(|| {
                format!(
                    "https://news.ycombinator.com/item?id={}",
                    h.object_id.as_deref().unwrap_or("")
                )
            }),
            author: h.author.unwrap_or_default(),
            points: h.points.unwrap_or(0),
            num_comments: h.num_comments.unwrap_or(0),
            created_at: h.created_at.unwrap_or_default(),
            position: idx + 1,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<SearchHit> {
        let parsed: HnResponse = serde_json::from_str(body).expect("parse");
        parsed
            .hits
            .into_iter()
            .filter(|h| h.title.is_some())
            .enumerate()
            .map(|(idx, h)| SearchHit::Story {
                title: h.title.unwrap_or_default(),
                url: h.url.unwrap_or_else(|| {
                    format!(
                        "https://news.ycombinator.com/item?id={}",
                        h.object_id.as_deref().unwrap_or("")
                    )
                }),
                author: h.author.unwrap_or_default(),
                points: h.points.unwrap_or(0),
                num_comments: h.num_comments.unwrap_or(0),
                created_at: h.created_at.unwrap_or_default(),
                position: idx + 1,
            })
            .collect()
    }

    #[test]
    fn parses_hn_sample() {
        let body = r#"{
            "hits": [
                {
                    "title": "Show HN: My Rust async runtime",
                    "url": "https://github.com/x/y",
                    "author": "rustfan",
                    "points": 142,
                    "num_comments": 37,
                    "created_at": "2026-05-30T12:00:00Z",
                    "object_id": "12345"
                },
                {
                    "title": "Ask HN: Best async runtimes 2026?",
                    "url": null,
                    "author": "curious",
                    "points": 58,
                    "num_comments": 22,
                    "created_at": "2026-05-29T08:00:00Z",
                    "object_id": "12346"
                }
            ]
        }"#;
        let hits = parse(body);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Story { title, points, .. } => {
                assert_eq!(title, "Show HN: My Rust async runtime");
                assert_eq!(*points, 142);
            }
            other => panic!("expected story, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Story { url, position, .. } => {
                assert!(url.contains("news.ycombinator.com/item?id=12346")); // null → fallback
                assert_eq!(*position, 2);
            }
            other => panic!("expected story, got {:?}", other),
        }
    }

    #[test]
    fn empty_hn_response() {
        let body = r#"{"hits": []}"#;
        assert!(parse(body).is_empty());
    }
}
