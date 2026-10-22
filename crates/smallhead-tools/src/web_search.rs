//! 5 个 web source 各自实现 `SearchProvider`,俾 unified `search` tool 用
//! (CJK-aware chain 喺 `web_search_tool.rs` 拣)。
//!
//! - zh query: Baidu → cn.bing.com → Mojeek → DDG
//! - en query: www.bing.com → Mojeek → DDG
//!
//! 所有 source 都用 `run_fallback_chain` 跑,自带健康度过滤 + 失败记录。

use async_trait::async_trait;

use crate::html_utils::{collapse_whitespace, strip_html};
use crate::http_client::build_client;
use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

// ── Mojeek (primary) ──────────────────────────────────────────────────
const MOJEEK_ENDPOINT: &str = "https://www.mojeek.com/search";
const MOJEEK_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

// ── DuckDuckGo (fallback) ─────────────────────────────────────────────
const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
// 用真浏览器嘅 Safari UA — DDG 嘅 anomaly detection 会 block 简单嘅 headless UA。
const DDG_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

// ── Bing (cn + com) ─────────────────────────────────────────────────
const BING_CN_ENDPOINT: &str = "https://cn.bing.com/search";
const BING_COM_ENDPOINT: &str = "https://www.bing.com/search";
const BING_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.15.7";

// ── Baidu ──────────────────────────────────────────────────────────
const BAIDU_ENDPOINT: &str = "https://www.baidu.com/s";
const BAIDU_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.15.7";

// ════════════════════════════════════════════════════════════════════
// Provider impls —— 5 个 source,每个一个 struct + impl SearchProvider
// ════════════════════════════════════════════════════════════════════

/// Mojeek web search provider.
pub struct MojeekProvider;
/// DuckDuckGo HTML web search provider.
pub struct DuckDuckGoProvider;
/// Bing China web search provider.
pub struct BingCnProvider;
/// Bing global web search provider.
pub struct BingComProvider;
/// Baidu web search provider.
pub struct BaiduProvider;

#[async_trait]
impl SearchProvider for MojeekProvider {
    fn name(&self) -> &'static str {
        "mojeek"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_mojeek(query, limit).await
    }
}

#[async_trait]
impl SearchProvider for DuckDuckGoProvider {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_ddg(query, limit).await
    }
}

#[async_trait]
impl SearchProvider for BingCnProvider {
    fn name(&self) -> &'static str {
        "bing-cn"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_bing(query, limit, BING_CN_ENDPOINT, "zh-CN").await
    }
}

#[async_trait]
impl SearchProvider for BingComProvider {
    fn name(&self) -> &'static str {
        "bing"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_bing(query, limit, BING_COM_ENDPOINT, "en-US").await
    }
}

#[async_trait]
impl SearchProvider for BaiduProvider {
    fn name(&self) -> &'static str {
        "baidu"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        search_baidu(query, limit).await
    }
}

// ════════════════════════════════════════════════════════════════════
// Mojeek (primary) — 简单 HTML,每条 result 系一个 <li class="r*">,
// title 喺 <h2><a class="title">,URL 喺 <a class="ob">,snippet 喺 <p class="s">。
// ════════════════════════════════════════════════════════════════════

async fn search_mojeek(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let client = build_client(MOJEEK_USER_AGENT)?;

    let body = client
        .get(MOJEEK_ENDPOINT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    Ok(parse_mojeek_html(&body, limit))
}

fn parse_mojeek_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut cursor = 0usize;
    let mut position = 1usize;

    while let Some(rel_idx) = html[cursor..].find("class=\"title\"") {
        let abs = cursor + rel_idx;
        // 跳回 <h2> 容器: 找前一个 <a ...> 开始
        // 更简单: 找 href 直接定位
        let url = match extract_mojeek_url(&html[abs..]) {
            Some(u) => u,
            None => break,
        };

        // 找 <a class="title" ...>...</a> 的内容做 title
        let anchor_open_end = match html[abs..].find('>') {
            Some(o) => abs + o + 1,
            None => break,
        };
        let anchor_close_rel = match html[anchor_open_end..].find("</a>") {
            Some(c) => c,
            None => break,
        };
        let raw_title = &html[anchor_open_end..anchor_open_end + anchor_close_rel];
        let title = collapse_whitespace(&strip_html(raw_title));

        // snippet: 找后续嘅 <p class="s">...</p>
        let after = anchor_open_end + anchor_close_rel;
        let description = extract_mojeek_snippet(&html[after..]).unwrap_or_default();

        if !url.is_empty() && !title.is_empty() {
            hits.push(SearchHit::Web {
                title,
                url,
                description,
                position,
            });
            position += 1;
            if hits.len() >= limit {
                break;
            }
        }

        cursor = after;
        if cursor >= html.len() {
            break;
        }
    }

    hits
}

fn extract_mojeek_url(segment: &str) -> Option<String> {
    // 找 "title=\"URL\""  —— Mojeek 嘅 <a class="title" title="https://..." href="https://...">
    let title_marker = "title=\"";
    let title_start = segment.find(title_marker)? + title_marker.len();
    let title_end_rel = segment[title_start..].find('"')?;
    let url = &segment[title_start..title_start + title_end_rel];
    if url.starts_with("http") {
        Some(url.to_string())
    } else {
        None
    }
}

fn extract_mojeek_snippet(segment: &str) -> Option<String> {
    let s_marker = "class=\"s\"";
    let s_pos = segment.find(s_marker)?;
    let after_marker = &segment[s_pos + s_marker.len()..];
    let open_end = after_marker.find('>')? + 1;
    let body = &after_marker[open_end..];
    let close_rel = body.find("</p>")?;
    Some(collapse_whitespace(&strip_html(&body[..close_rel])))
}

// ════════════════════════════════════════════════════════════════════
// DuckDuckGo (fallback) —— 旧嘅 parser,header 维持原样。
// ════════════════════════════════════════════════════════════════════

async fn search_ddg(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let client = build_client(DDG_USER_AGENT)?;

    let body = client
        .post(DDG_ENDPOINT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7")
        .header("Referer", "https://duckduckgo.com/")
        .form(&[("q", query), ("kl", "us-en")])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    if body.contains("anomaly-modal") || body.contains("challenge-form") {
        return Err("DuckDuckGo returned anti-bot challenge (CAPTCHA)".into());
    }

    Ok(parse_ddg_html(&body, limit))
}

fn parse_ddg_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut cursor = 0usize;
    let mut position = 1usize;

    while let Some(rel_idx) = html[cursor..].find("class=\"result__a\"") {
        let abs = cursor + rel_idx;
        let anchor_open_end = match html[abs..].find('>') {
            Some(o) => abs + o + 1,
            None => break,
        };
        let anchor_close_rel = match html[anchor_open_end..].find("</a>") {
            Some(c) => c,
            None => break,
        };
        let raw_title = &html[anchor_open_end..anchor_open_end + anchor_close_rel];
        let title = collapse_whitespace(&strip_html(raw_title));

        let href = extract_ddg_href(&html[abs..]);
        let url = href.unwrap_or_default();

        let snippet_start = match html[anchor_open_end..].find("class=\"result__snippet\"") {
            Some(s) => anchor_open_end + s,
            None => break,
        };
        let snippet_open_end = match html[snippet_start..].find('>') {
            Some(o) => snippet_start + o + 1,
            None => break,
        };
        let snippet_close_rel = match html[snippet_open_end..].find("</a>") {
            Some(c) => c,
            None => break,
        };
        let raw_desc = &html[snippet_open_end..snippet_open_end + snippet_close_rel];
        let description = collapse_whitespace(&strip_html(raw_desc));

        if !url.is_empty() {
            hits.push(SearchHit::Web {
                title,
                url,
                description,
                position,
            });
            position += 1;
            if hits.len() >= limit {
                break;
            }
        }

        cursor = snippet_open_end + snippet_close_rel;
        if cursor >= html.len() {
            break;
        }
    }

    hits
}

fn extract_ddg_href(segment: &str) -> Option<String> {
    let href_start = segment.find("href=\"")? + "href=\"".len();
    let href_end_rel = segment[href_start..].find('"')?;
    let raw = &segment[href_start..href_start + href_end_rel];
    decode_ddg_redirect(raw)
}

fn decode_ddg_redirect(href: &str) -> Option<String> {
    let marker = "uddg=";
    let idx = href.find(marker)?;
    let after = &href[idx + marker.len()..];
    let amp = after.find('&').unwrap_or(after.len());
    let encoded = &after[..amp];
    let decoded = percent_decode(encoded);
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ════════════════════════════════════════════════════════════════════
// Bing (cn.bing.com / www.bing.com) — 简单 HTML,每条 result 系
// <li class="b_algo">,title 喺 <h2><a href="...">,snippet 喺 <div class="b_caption">。
// ════════════════════════════════════════════════════════════════════

async fn search_bing(
    query: &str,
    limit: usize,
    endpoint: &str,
    lang: &str,
) -> Result<Vec<SearchHit>, String> {
    let client = build_client(BING_USER_AGENT)?;

    let body = client
        .get(endpoint)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", lang)
        .query(&[("q", query), ("count", limit.to_string().as_str())])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    Ok(parse_bing_html(&body, limit))
}

fn parse_bing_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut position = 1usize;
    let mut cursor = 0usize;

    while let Some(rel_idx) = html[cursor..].find("class=\"b_algo\"") {
        let abs = cursor + rel_idx;
        // Extract URL from the first <a href="..."> within this <li> block.
        // We scan to the next </li> to bound the block.
        let block_end = html[abs..]
            .find("</li>")
            .map(|n| abs + n)
            .unwrap_or(html.len());
        let block = &html[abs..block_end];

        let url = match extract_first_href(block) {
            Some(u) if u.starts_with("http") => u,
            _ => {
                cursor = block_end;
                continue;
            }
        };

        // Title: <h2><a ...>TITLE</a></h2> — find first <h2> block in the result.
        let title = extract_bing_title(block).unwrap_or_default();

        // Snippet: <div class="b_caption"><p ...>SNIPPET</p></div>
        let description = extract_bing_snippet(block).unwrap_or_default();

        if !title.is_empty() {
            hits.push(SearchHit::Web {
                title: collapse_whitespace(&strip_html(&title)),
                url,
                description: collapse_whitespace(&strip_html(&description)),
                position,
            });
            position += 1;
            if hits.len() >= limit {
                break;
            }
        }
        cursor = block_end;
    }

    hits
}

fn extract_first_href(segment: &str) -> Option<String> {
    let href_start = segment.find("href=\"")? + "href=\"".len();
    let href_end_rel = segment[href_start..].find('"')?;
    Some(segment[href_start..href_start + href_end_rel].to_string())
}

fn extract_bing_title(segment: &str) -> Option<String> {
    let h2_start = segment.find("<h2")?;
    let after_h2 = &segment[h2_start..];
    let a_start = after_h2.find("<a")?;
    let gt = after_h2[a_start..].find('>')?;
    let content_start = a_start + gt + 1;
    let a_end = after_h2[content_start..].find("</a>")?;
    Some(after_h2[content_start..content_start + a_end].to_string())
}

fn extract_bing_snippet(segment: &str) -> Option<String> {
    let marker = "<div class=\"b_caption\"";
    let cap_start = segment.find(marker)?;
    let after_cap = &segment[cap_start..];
    let p_start = after_cap.find("<p")?;
    let gt = after_cap[p_start..].find('>')?;
    let content_start = p_start + gt + 1;
    let p_end = after_cap[content_start..].find("</p>")?;
    Some(after_cap[content_start..content_start + p_end].to_string())
}

// ════════════════════════════════════════════════════════════════════
// Baidu (www.baidu.com) — HTML 结构变种多,我们用通用选择器:
// 1) 找 <h3 class="..."> 容器作为 title 块
// 2) 找第一个外链 <a href="http..."> 作为 URL
// 3) 找最近一个含摘要文本的 <span> 或 <div class="c-abstract*"> 作为 snippet
// ════════════════════════════════════════════════════════════════════

async fn search_baidu(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let client = build_client(BAIDU_USER_AGENT)?;

    let body = client
        .get(BAIDU_ENDPOINT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .query(&[("wd", query), ("rn", limit.to_string().as_str())])
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .error_for_status()
        .map_err(|e| format!("status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body: {e}"))?;

    Ok(parse_baidu_html(&body, limit))
}

fn parse_baidu_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut position = 1usize;
    let mut cursor = 0usize;

    // Baidu modern layout uses <h3 class="..."><a href="URL">TITLE</a></h3>
    // as the title block. Iterate all h3 blocks, then for each find
    // the enclosing result container by scanning outward to the next
    // "result" / "c-container" ancestor — but for simplicity we treat
    // each <h3>...</h3> block as one result.
    while let Some(rel_idx) = html[cursor..].find("<h3") {
        let abs = cursor + rel_idx;
        // Find the end of <h3 ...>
        let gt = match html[abs..].find('>') {
            Some(n) => abs + n + 1,
            None => break,
        };
        let h3_end = match html[gt..].find("</h3>") {
            Some(n) => gt + n,
            None => break,
        };
        let h3_block = &html[gt..h3_end];

        // URL: first href="http" inside h3 (skip baidu-internal redirects)
        let url = h3_block.find("href=\"").and_then(|href_start| {
            let after = &h3_block[href_start + "href=\"".len()..];
            let href_end = after.find('"')?;
            let raw = &after[..href_end];
            if raw.starts_with("http") {
                Some(raw.to_string())
            } else {
                None
            }
        });

        // Title: text inside the <a> within h3
        let title = h3_block
            .find("<a")
            .and_then(|a_start| {
                let after = &h3_block[a_start..];
                let gt = after.find('>')? + 1;
                let a_end = after[gt..].find("</a>")?;
                Some(after[gt..gt + a_end].to_string())
            })
            .unwrap_or_default();

        if let Some(url) = url {
            if !title.is_empty() {
                hits.push(SearchHit::Web {
                    title: collapse_whitespace(&strip_html(&title)),
                    url,
                    description: String::new(), // snippet extraction is layout-fragile; skip for v1
                    position,
                });
                position += 1;
                if hits.len() >= limit {
                    break;
                }
            }
        }
        cursor = h3_end + "</h3>".len();
    }

    hits
}

#[allow(dead_code)]
const _DEFAULT_LIMIT_USED: usize = DEFAULT_LIMIT;
#[allow(dead_code)]
const _MAX_LIMIT_USED: usize = MAX_LIMIT;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DDG: &str = r#"<html><body>
        <div class="result">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone&amp;rut=abc">Example One</a>
            <a class="result__snippet">First description here.</a>
        </div>
        <div class="result">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ftwo&amp;rut=def">Example Two</a>
            <a class="result__snippet">Second <b>description</b> with tags.</a>
        </div>
    </body></html>"#;

    const SAMPLE_MOJEEK: &str = r#"<html><body><ul class="results">
        <li class="r7 clu-result"><a title="https://www.tesla.com/modely" href="https://www.tesla.com/modely" class="ob"><p class="i"><span class="url">https://www.tesla.com<span> › modely</span></span></p></a><h2><a class="title" title="https://www.tesla.com/modely" href="https://www.tesla.com/modely/">Model Y | Tesla</a></h2><p class="s">The <strong>Model Y</strong> is a battery electric compact crossover SUV.</p><p class="more"><a href="/search?q=site%3Atesla.com+Model+Y">See more</a></p></li>
        <li class="r8"><a title="https://insideevs.com/tesla/model-y/" href="https://insideevs.com/tesla/model-y/" class="ob"><p class="i"><span class="url">https://insideevs.com<span> › tesla › model-y</span></span></p></a><h2><a class="title" title="https://insideevs.com/tesla/model-y/" href="https://insideevs.com/tesla/model-y/">Tesla Model Y News and Reviews | InsideEVs</a></h2><p class="s">Get the latest on new <strong>Tesla</strong> <strong>Model</strong> <strong>Y</strong> models including first drives, in-depth reviews.</p><p class="more"><a href="/search?q=site%3Ainsideevs.com+Tesla+Model+Y">See more</a></p></li>
    </ul></body></html>"#;

    #[test]
    fn parses_mojeek_sample() {
        let hits = parse_mojeek_html(SAMPLE_MOJEEK, 10);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web {
                url,
                title,
                description,
                ..
            } => {
                assert_eq!(url, "https://www.tesla.com/modely");
                assert!(title.contains("Model Y"));
                assert!(description.contains("crossover"));
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Web { position, .. } => assert_eq!(*position, 2),
            other => panic!("expected web hit, got {:?}", other),
        }
    }

    #[test]
    fn mojeek_respects_limit() {
        let hits = parse_mojeek_html(SAMPLE_MOJEEK, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn parses_ddg_sample() {
        let hits = parse_ddg_html(SAMPLE_DDG, 10);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web {
                title,
                url,
                position,
                ..
            } => {
                assert_eq!(title, "Example One");
                assert_eq!(url, "https://example.com/one");
                assert_eq!(*position, 1);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Web {
                url,
                position,
                description,
                ..
            } => {
                assert_eq!(url, "https://example.com/two");
                assert_eq!(*position, 2);
                assert!(description.contains("Second"));
            }
            other => panic!("expected web hit, got {:?}", other),
        }
    }

    #[test]
    fn ddg_respects_limit() {
        let hits = parse_ddg_html(SAMPLE_DDG, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn percent_decode_handles_plus_and_escapes() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
    }

    #[test]
    fn html_utils_strip_and_collapse() {
        use crate::html_utils::{collapse_whitespace, strip_html};
        // 剥标签
        assert_eq!(strip_html("a <b>b</b> c"), "a b c");
        // 合并空白
        assert_eq!(collapse_whitespace("  multi   space  "), "multi space");
        // 组合:这是原来 `strip_html_tags` 嘅行为
        assert_eq!(collapse_whitespace(&strip_html("a <b>b</b> c")), "a b c");
    }

    #[test]
    fn classify_error_distinguishes_transport_and_content() {
        // 旧 `is_transport_error` 嘅逻辑已经搬入 `provider_health::classify_error`。
        // 保留呢个 test 锁住 transport 关键字嘅 detection,避免以后 regression。
        use crate::provider_health::classify_error;
        assert!(matches!(
            classify_error("error sending request for url"),
            crate::provider_health::ProviderErrorKind::Timeout
        ));
        assert!(matches!(
            classify_error("Connection refused"),
            crate::provider_health::ProviderErrorKind::Timeout
        ));
        assert!(matches!(
            classify_error("dns error: no such host"),
            crate::provider_health::ProviderErrorKind::Timeout
        ));
        assert!(matches!(
            classify_error("request timeout exceeded"),
            crate::provider_health::ProviderErrorKind::Timeout
        ));
        assert!(matches!(
            classify_error("status: 403 Forbidden"),
            crate::provider_health::ProviderErrorKind::CjkRejected
        ));
        assert!(matches!(
            classify_error("status: 429 Too Many Requests"),
            crate::provider_health::ProviderErrorKind::HttpStatus(429)
        ));
        // 解析错误唔系 transport
        assert!(matches!(
            classify_error("parse json failed"),
            crate::provider_health::ProviderErrorKind::ParseError
        ));
    }

    const SAMPLE_BING: &str = r#"<html><body><ol id="b_results"><li class="b_algo">
        <h2><a href="https://example.com/one">Example One Title</a></h2>
        <div class="b_caption"><p class="b_lineclamp2">First snippet about example one.</p></div>
    </li><li class="b_algo">
        <h2><a href="https://example.com/two">Second <b>Bold</b> Title</a></h2>
        <div class="b_caption"><p class="b_lineclamp2">Second snippet text here.</p></div>
    </li></ol></body></html>"#;

    #[test]
    fn parses_bing_sample() {
        let hits = parse_bing_html(SAMPLE_BING, 10);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web {
                title,
                url,
                position,
                description,
                ..
            } => {
                assert_eq!(title, "Example One Title");
                assert_eq!(url, "https://example.com/one");
                assert_eq!(*position, 1);
                assert!(description.contains("First snippet"));
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Web {
                title,
                url,
                position,
                ..
            } => {
                assert_eq!(url, "https://example.com/two");
                assert!(
                    title.contains("Bold"),
                    "bold tag should be stripped, got: {}",
                    title
                );
                assert_eq!(*position, 2);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
    }

    #[test]
    fn bing_respects_limit() {
        let hits = parse_bing_html(SAMPLE_BING, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn empty_html_returns_no_hits_for_bing() {
        assert!(parse_bing_html("", 10).is_empty());
        assert!(parse_bing_html("<html><body>no b_algo here</body></html>", 10).is_empty());
    }

    const SAMPLE_BAIDU: &str = r#"<html><body>
    <div class="result-op c-container">
        <h3 class="t"><a href="https://baike.baidu.com/item/Hello">hello（词语释义）</a></h3>
    </div>
    <div class="result-op c-container">
        <h3 class="t"><a href="https://zh.wikipedia.org/wiki/Hello">Hello - 维基百科</a></h3>
    </div>
    <div class="result-op c-container">
        <h3 class="t"><a href="https://example.com/three">第三条</a></h3>
    </div>
    </body></html>"#;

    #[test]
    fn parses_baidu_sample() {
        let hits = parse_baidu_html(SAMPLE_BAIDU, 10);
        assert_eq!(hits.len(), 3);
        match &hits[0] {
            SearchHit::Web {
                url,
                title,
                position,
                ..
            } => {
                assert_eq!(url, "https://baike.baidu.com/item/Hello");
                assert!(title.contains("hello"));
                assert_eq!(*position, 1);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[1] {
            SearchHit::Web { url, position, .. } => {
                assert_eq!(url, "https://zh.wikipedia.org/wiki/Hello");
                assert_eq!(*position, 2);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
        match &hits[2] {
            SearchHit::Web { url, position, .. } => {
                assert_eq!(url, "https://example.com/three");
                assert_eq!(*position, 3);
            }
            other => panic!("expected web hit, got {:?}", other),
        }
    }

    #[test]
    fn baidu_respects_limit() {
        let hits = parse_baidu_html(SAMPLE_BAIDU, 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn empty_html_returns_no_hits_for_baidu() {
        assert!(parse_baidu_html("", 10).is_empty());
        assert!(parse_baidu_html("<html><body>no h3 tags</body></html>", 10).is_empty());
    }

    #[test]
    fn empty_html_returns_no_hits() {
        assert!(parse_ddg_html("", 10).is_empty());
        assert!(parse_ddg_html("<html><body>no results</body></html>", 10).is_empty());
        assert!(parse_mojeek_html("", 10).is_empty());
        assert!(parse_mojeek_html("<html><body>no results</body></html>", 10).is_empty());
    }

    #[test]
    fn provider_names_locked() {
        assert_eq!(MojeekProvider.name(), "mojeek");
        assert_eq!(DuckDuckGoProvider.name(), "duckduckgo");
        assert_eq!(BingCnProvider.name(), "bing-cn");
        assert_eq!(BingComProvider.name(), "bing");
        assert_eq!(BaiduProvider.name(), "baidu");
        assert_eq!(MojeekProvider.kind(), SourceKind::Web);
        assert_eq!(BaiduProvider.kind(), SourceKind::Web);
    }
}
