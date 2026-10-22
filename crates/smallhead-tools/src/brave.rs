//! `BraveProvider` — Brave 搜索引擎（独立 index,无 anti-bot）。
//!
//! Brave 嘅 HTML 用 Svelte 渲染,结构复杂：
//! - Result 块:`<div data-type="web" ...>`(一个 result 一个)
//! - Title link:`<a class="...l1..." href="URL">TITLE</a>`
//! - Snippet:`<... class="snippet ...">SNIPPET</...>`(可能嵌套 enrichment cards)
//!
//! 我们只抽 title + URL + 简单 snippet。snippet 抽取 best-effort，
//! 如果 Svelte 哈希变化,只返 title+URL 仍可用。
//!
//! Phase 3 之后唔再有 `brave_search` tool —— LLM 改用 unified `search` tool 加 `kind=web`。
//! Brave 仍喺 web 链 fallback 顺序中（按 health registry 决定）。

use async_trait::async_trait;

use crate::html_utils::{collapse_whitespace, strip_html};
use crate::http_client::build_client;
use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

const BRAVE_ENDPOINT: &str = "https://search.brave.com/search";
const BRAVE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.15.7";

/// Brave web search provider.
pub struct BraveProvider;

#[async_trait]
impl SearchProvider for BraveProvider {
    fn name(&self) -> &'static str {
        "brave"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_brave(query, limit).await
    }
}

async fn search_brave(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let client = build_client(BRAVE_USER_AGENT)?;

    let body = client
        .get(BRAVE_ENDPOINT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7")
        .query(&[("q", query), ("source", "web")])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    Ok(parse_brave_html(&body, limit))
}

fn parse_brave_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut position = 1usize;

    // 找所有带 l1 class 的外链 title 块（每个 result 一个）
    let link_re = match regex::Regex::new(r#"(?s)<a\s+[^>]*class="[^"]*\bl1\b[^"]*"[^>]*>.*?</a>"#)
    {
        Ok(re) => re,
        Err(_) => return hits,
    };
    for m in link_re.find_iter(html) {
        let raw = m.as_str();
        let Some(url) = extract_href(raw) else {
            continue;
        };

        // 过滤 brave.com 内部链接
        if url.contains("brave.com") {
            continue;
        }

        // Title: 拿 `>` 之后到 `</a>` 之前的纯文本（跳过 site-name / favicon 容器）
        let title = extract_title_from_link(raw);
        if title.is_empty() {
            continue;
        }

        // Snippet: 在 link 结束位置之后找下一个 class="snippet..." 块
        let snippet = find_snippet_after(html, m.end());

        hits.push(SearchHit::Web {
            title,
            url,
            description: snippet,
            position,
        });
        position += 1;
        if hits.len() >= limit {
            break;
        }
    }

    hits
}

/// 从 `<a href="URL" class="...">...</a>` 整段里抽 href 值。
fn extract_href(a_tag: &str) -> Option<String> {
    let href_pos = a_tag.find("href=")?;
    let after_eq = &a_tag[href_pos + 5..];
    let bytes = after_eq.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0];
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &after_eq[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// 从 `<a ...>INNER</a>` 整段里抽 title 纯文本。
/// 取第一个 `>` 之后到最后一个 `</a>` 之间的内容,strip tags + collapse whitespace。
fn extract_title_from_link(a_tag: &str) -> String {
    let Some(open_end) = a_tag.find('>') else {
        return String::new();
    };
    let Some(close_start) = a_tag.rfind("</a>") else {
        return String::new();
    };
    if close_start <= open_end {
        return String::new();
    }
    let inner = &a_tag[open_end + 1..close_start];
    collapse_whitespace(&strip_html(inner))
}

fn find_snippet_after(html: &str, after: usize) -> String {
    // 找下一个 class="snippet..." block
    let segment = &html[after..];
    let Some(snippet_start) = segment.find("class=\"snippet") else {
        return String::new();
    };
    // snippet div 结束于下一个 </div> 平衡的位置;简化做法:取 2000 chars,strip tags
    let after_snippet = &segment[snippet_start..];
    let bound = after_snippet.len().min(2000);
    let collapsed = collapse_whitespace(&strip_html(&after_snippet[..bound]));
    // 只取第一段有意义的话——通常 snippet 头 200 字符够用
    collapsed.chars().take(250).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BRAVE: &str = r#"<html><body>
        <div data-type="web">
            <a href="https://tokio.rs/" target="_self" class="svelte-14r20fy l1">
                <div class="site-name">Tokio tokio.rs</div>
                Tokio - An asynchronous Rust runtime
            </a>
            <div class="snippet  svelte-jmfu5f standalone">
                Tokio is a runtime for writing reliable asynchronous applications with Rust. It provides async I/O, networking, scheduling, timers.
            </div>
        </div>
        <div data-type="web">
            <a href="https://github.com/tokio-rs/tokio" class="svelte-14r20fy l1">
                <div class="site-name">GitHub github.com</div>
                GitHub - tokio-rs/tokio: A runtime for writing reliable asynchronous applications
            </a>
            <div class="snippet  svelte-jmfu5f">
                A runtime for writing reliable asynchronous applications with Rust. Provides I/O, networking, scheduling.
            </div>
        </div>
    </body></html>"#;

    #[test]
    fn parses_brave_sample() {
        let hits = parse_brave_html(SAMPLE_BRAVE, 10);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web {
                title,
                url,
                description,
                position,
                ..
            } => {
                assert_eq!(url, "https://tokio.rs/");
                assert!(
                    title.contains("Tokio - An asynchronous Rust runtime"),
                    "title got: {}",
                    title
                );
                assert!(
                    description.contains("runtime for writing reliable"),
                    "snippet got: {}",
                    description
                );
                assert_eq!(*position, 1);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Web { url, position, .. } => {
                assert_eq!(url, "https://github.com/tokio-rs/tokio");
                assert_eq!(*position, 2);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
    }

    #[test]
    fn brave_respects_limit() {
        let hits = parse_brave_html(SAMPLE_BRAVE, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn brave_skips_brave_internal_links() {
        let html = r#"<a href="https://search.brave.com/help" class="l1">Help</a>"#;
        let hits = parse_brave_html(html, 5);
        assert!(hits.is_empty(), "should skip brave.com internal links");
    }

    #[test]
    fn empty_brave_html_returns_no_hits() {
        assert!(parse_brave_html("", 10).is_empty());
        assert!(parse_brave_html("<html><body>no l1 links</body></html>", 10).is_empty());
    }
}
