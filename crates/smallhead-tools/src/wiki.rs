//! `WikipediaProvider` — Wikipedia / 中文维基百科搜索 + summary。
//!
//! 走 MediaWiki REST API + Action API,纯 JSON,无 anti-bot。
//! - 搜索: `https://{lang}.wikipedia.org/w/api.php?action=query&list=search&srsearch=...&format=json`
//! - Summary: `https://{lang}.wikipedia.org/api/rest_v1/page/summary/{title}`
//!
//! 支持 en / zh 两种 wiki。Agent 嘅 query 包含中文就用 zh,否则默认 en(可由调用方覆盖)。
//!
//! Phase 1: `WikipediaProvider` 实现 `SearchProvider` trait,统一返 `SearchHit::Wiki` 变体。
//! 旧 `WikiHit` 嘅 `pageid` 字段被砍(LLM 唔需要 Wikipedia 内部 ID)。
//!
//! Phase 3 之后唔再有 `wiki_search` tool —— LLM 改用 unified `search` tool 加 `kind=wiki`。
//! `chain_for_kind` 会按 query 是否含 CJK 自动拣 zh / en variant。

use async_trait::async_trait;
use serde::Deserialize;

use crate::html_utils::{collapse_whitespace, strip_html};
use crate::http_client::build_client;
use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

/// Wikipedia search provider for a fixed language.
pub struct WikipediaProvider {
    lang: &'static str,
}

static WIKIPEDIA_EN: WikipediaProvider = WikipediaProvider { lang: "en" };
static WIKIPEDIA_ZH: WikipediaProvider = WikipediaProvider { lang: "zh" };

impl WikipediaProvider {
    /// English Wikipedia provider instance.
    pub fn english() -> &'static Self {
        &WIKIPEDIA_EN
    }
    /// Chinese Wikipedia provider instance.
    pub fn chinese() -> &'static Self {
        &WIKIPEDIA_ZH
    }
}

#[async_trait]
impl SearchProvider for WikipediaProvider {
    fn name(&self) -> &'static str {
        "wikipedia"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Wiki
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_wikipedia(query, self.lang, limit).await
    }
}

#[allow(dead_code)]
fn detect_lang(query: &str) -> String {
    // 简单启发:含 CJK 字符就 zh,否则 en
    if query.chars().any(|c| {
        let cp = c as u32;
        (0x4E00..=0x9FFF).contains(&cp) ||  // CJK Unified Ideographs
        (0x3400..=0x4DBF).contains(&cp) // CJK Ext A
    }) {
        "zh".to_string()
    } else {
        "en".to_string()
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    query: Option<ApiQuery>,
}

#[derive(Debug, Deserialize)]
struct ApiQuery {
    search: Vec<ApiSearchEntry>,
}

#[derive(Debug, Deserialize)]
struct ApiSearchEntry {
    title: String,
    #[allow(dead_code)]
    pageid: u64,
    #[serde(rename = "snippet")]
    snippet_html: String,
}

async fn search_wikipedia(query: &str, lang: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let lang_path = if lang == "zh" { "zh" } else { "en" };
    let endpoint = format!("https://{lang_path}.wikipedia.org/w/api.php");

    let client =
        build_client("smallhead/0.1 (https://github.com/weykon/small-head; agent runtime)")?;

    let body = client
        .get(&endpoint)
        .header("Accept", "application/json")
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", query),
            ("srlimit", &limit.to_string()),
            ("format", "json"),
        ])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    let parsed: ApiResponse = serde_json::from_str(&body).map_err(|e| {
        format!(
            "parse json: {e}; body[:200]={}",
            &body[..body.len().min(200)]
        )
    })?;

    let entries = parsed.query.map(|q| q.search).unwrap_or_default();

    let hits = entries
        .into_iter()
        .enumerate()
        .map(|(idx, e)| {
            let url = format!(
                "https://{lang_path}.wikipedia.org/wiki/{}",
                urlencoding(&e.title)
            );
            SearchHit::Wiki {
                title: e.title,
                url,
                snippet: collapse_whitespace(&strip_html(&e.snippet_html)),
                lang: lang_path.to_string(),
                position: idx + 1,
            }
        })
        .collect();

    Ok(hits)
}

/// Minimal URL-encoding for wiki titles (spaces, slashes, etc.).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_chinese() {
        assert_eq!(detect_lang("特斯拉 Model Y"), "zh");
        assert_eq!(detect_lang("Tesla Model Y"), "en");
        assert_eq!(detect_lang("中華人民共和國"), "zh");
    }

    #[test]
    fn strips_wiki_html() {
        let s = r#"The <span class="searchmatch">Tesla</span> <span class="searchmatch">Model</span> Y is a..."#;
        let out = collapse_whitespace(&strip_html(s));
        assert_eq!(out, "The Tesla Model Y is a...");
    }

    #[test]
    fn urlencodes_titles() {
        assert_eq!(urlencoding("Tesla Model Y"), "Tesla%20Model%20Y");
        assert_eq!(urlencoding("C++"), "C%2B%2B");
        assert_eq!(urlencoding("Café"), "Caf%C3%A9");
    }

    #[test]
    fn provider_locales_locked() {
        assert_eq!(WikipediaProvider::english().name(), "wikipedia");
        assert_eq!(WikipediaProvider::chinese().name(), "wikipedia");
        assert_eq!(WikipediaProvider::english().kind(), SourceKind::Wiki);
        assert_eq!(WikipediaProvider::chinese().kind(), SourceKind::Wiki);
    }
}
