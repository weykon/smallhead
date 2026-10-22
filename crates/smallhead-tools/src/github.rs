//! `GithubProvider` — GitHub Repository Search (公开 API, 无 auth, JSON 干净)。
//!
//! Endpoint: `https://api.github.com/search/repositories?q={query}&per_page={limit}&sort=stars`
//! - 返回 structured JSON: full_name, html_url, description, stargazers_count, language
//! - 无 anti-bot;无 captcha
//! - 限速:未认证 60 req/h/IP — 一次 cycle 几 queries 安全
//!
//! Phase 1: `GithubProvider` 实现 `SearchProvider` trait,统一返 `SearchHit::Repo` 变体。
//! 旧 `RepoHit` struct 被砍,信封装喺 `SearchHit` enum(`kind: "repo"` tag)。
//!
//! Phase 3 之后唔再有 `github_search` tool —— LLM 改用 unified `search` tool 加 `kind=code`。
//! GitHub 系 Code 链嘅唯一 source,无需 fallback。

use async_trait::async_trait;
use serde::Deserialize;

use crate::http_client::build_client;
use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

const GITHUB_ENDPOINT: &str = "https://api.github.com/search/repositories";
const GITHUB_USER_AGENT: &str = "smallhead/0.1";

/// GitHub repository search provider.
pub struct GithubProvider;

#[async_trait]
impl SearchProvider for GithubProvider {
    fn name(&self) -> &'static str {
        "github"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Code
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_github(query, limit as u32).await
    }
}

#[derive(Debug, Deserialize)]
struct GithubResponse {
    items: Vec<GithubItem>,
    #[allow(dead_code)]
    total_count: u64,
}

#[derive(Debug, Deserialize)]
struct GithubItem {
    full_name: String,
    html_url: String,
    description: Option<String>,
    language: Option<String>,
    stargazers_count: u64,
    forks_count: u64,
}

async fn search_github(query: &str, limit: u32) -> Result<Vec<SearchHit>, String> {
    let client = build_client(GITHUB_USER_AGENT)?;

    let body = client
        .get(GITHUB_ENDPOINT)
        .header("Accept", "application/vnd.github+json")
        .query(&[
            ("q", query),
            ("per_page", &limit.to_string()),
            ("sort", "stars"),
            ("order", "desc"),
        ])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    let parsed: GithubResponse = serde_json::from_str(&body).map_err(|e| {
        format!(
            "parse json: {e}; body[:300]={}",
            &body[..body.len().min(300)]
        )
    })?;

    Ok(parsed
        .items
        .into_iter()
        .enumerate()
        .map(|(idx, item)| SearchHit::Repo {
            full_name: item.full_name,
            html_url: item.html_url,
            description: item.description.unwrap_or_default(),
            language: item.language.unwrap_or_default(),
            stars: item.stargazers_count,
            forks: item.forks_count,
            position: idx + 1,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<SearchHit> {
        let parsed: GithubResponse = serde_json::from_str(body).expect("parse");
        parsed
            .items
            .into_iter()
            .enumerate()
            .map(|(idx, item)| SearchHit::Repo {
                full_name: item.full_name,
                html_url: item.html_url,
                description: item.description.unwrap_or_default(),
                language: item.language.unwrap_or_default(),
                stars: item.stargazers_count,
                forks: item.forks_count,
                position: idx + 1,
            })
            .collect()
    }

    #[test]
    fn parses_github_sample() {
        let body = r#"{
            "total_count": 2,
            "items": [
                {
                    "full_name": "tokio-rs/tokio",
                    "html_url": "https://github.com/tokio-rs/tokio",
                    "description": "A runtime for writing reliable asynchronous applications with Rust",
                    "language": "Rust",
                    "stargazers_count": 28000,
                    "forks_count": 2600
                },
                {
                    "full_name": "rust-lang/rust",
                    "html_url": "https://github.com/rust-lang/rust",
                    "description": null,
                    "language": "Rust",
                    "stargazers_count": 95000,
                    "forks_count": 12000
                }
            ]
        }"#;
        let hits = parse(body);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Repo {
                full_name, stars, ..
            } => {
                assert_eq!(full_name, "tokio-rs/tokio");
                assert_eq!(*stars, 28000);
            }
            other => panic!("expected repo, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Repo {
                description,
                position,
                ..
            } => {
                assert_eq!(description, ""); // null → empty
                assert_eq!(*position, 2);
            }
            other => panic!("expected repo, got {:?}", other),
        }
    }

    #[test]
    fn empty_github_response() {
        let body = r#"{"total_count": 0, "items": []}"#;
        assert!(parse(body).is_empty());
    }
}
