//! Shared HTML utilities — kept tiny and `regex`-based so they compile fast
//! and have no JS / DOM dependencies.
//!
//! The core pair is [`strip_html`] + [`collapse_whitespace`]; together they
//! yield `"a <b>b</b> c" → "a b c"`.
//!
//! [`html_to_readable_text`] is a best-effort extraction for ordinary pages:
//! drops noisy blocks (`<script>` / `<style>` / etc.), inserts line breaks
//! around block tags, strips the remaining tags, decodes common entities,
//! and collapses whitespace.

use regex::Regex;

/// Strip `<...>` tags, keeping tag-exterior text intact (including whitespace).
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Collapse runs of whitespace (spaces / tabs / newlines) into a single space
/// and trim both ends.
pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Best-effort readable extraction for ordinary HTML pages.
///
/// Deliberately lightweight: no JS execution, no full DOM. Drops noisy
/// blocks, keeps text boundaries around block tags, strips remaining tags,
/// decodes common entities, and collapses whitespace.
pub fn html_to_readable_text(html: &str) -> String {
    let without_noise = remove_tag_blocks(html, &["script", "style", "noscript", "svg"]);
    let with_breaks = insert_block_breaks(&without_noise);
    collapse_whitespace(&decode_basic_entities(&strip_html(&with_breaks)))
}

/// Extract `<title>...</title>` if present.
pub fn extract_title(html: &str) -> Option<String> {
    let re = Regex::new("(?is)<title[^>]*>(.*?)</title>").ok()?;
    let raw = re.captures(html)?.get(1)?.as_str();
    let title = collapse_whitespace(&decode_basic_entities(&strip_html(raw)));
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Extract up to `max_links` anchor links from an HTML page.
pub fn extract_links(html: &str, max_links: usize) -> Vec<(String, String)> {
    let Ok(re) = Regex::new("(?is)<a\\b[^>]*?href=[\"']([^\"']+)[\"'][^>]*>(.*?)</a>") else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for cap in re.captures_iter(html) {
        let Some(href) = cap.get(1).map(|m| m.as_str().trim()) else {
            continue;
        };
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let text = cap
            .get(2)
            .map(|m| collapse_whitespace(&decode_basic_entities(&strip_html(m.as_str()))))
            .unwrap_or_default();
        links.push((text, href.to_string()));
        if links.len() >= max_links {
            break;
        }
    }
    links
}

fn remove_tag_blocks(html: &str, tags: &[&str]) -> String {
    let mut out = html.to_string();
    for tag in tags {
        let pattern = format!("(?is)<{tag}\\b[^>]*>.*?</{tag}>");
        if let Ok(re) = Regex::new(&pattern) {
            out = re.replace_all(&out, " ").to_string();
        }
    }
    out
}

fn insert_block_breaks(html: &str) -> String {
    let Ok(re) = Regex::new(
        "(?i)</?(p|div|section|article|main|header|footer|nav|aside|h[1-6]|li|ul|ol|table|tr|td|th|br)\\b[^>]*>",
    ) else {
        return html.to_string();
    };
    re.replace_all(html, "\n").to_string()
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_text_removes_scripts_and_keeps_text() {
        let html = r#"
          <html><head><title>Hello & Test</title><script>bad()</script></head>
          <body><nav>menu</nav><main><h1>Hello</h1><p>A <b>page</b>.</p></main></body></html>
        "#;
        let text = html_to_readable_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("A page."));
        assert!(!text.contains("bad()"));
        assert_eq!(extract_title(html).as_deref(), Some("Hello & Test"));
    }

    #[test]
    fn extracts_links() {
        let links = extract_links(
            r##"<a href="https://example.com">Example <b>Site</b></a><a href="#x">skip</a>"##,
            10,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, "Example Site");
        assert_eq!(links[0].1, "https://example.com");
    }

    #[test]
    fn strip_html_removes_tags_keeps_text() {
        assert_eq!(strip_html("a <b>b</b> c"), "a b c");
    }

    #[test]
    fn collapse_whitespace_squashes_runs() {
        assert_eq!(collapse_whitespace("  multi   space  "), "multi space");
    }
}
