//! `agent_reach` — Agent-Reach provider bridging 16 channels via shell-out.
//!
//! Why shell-out (instead of pure-Rust like the 5 web providers in
//! [`crate::web_search`])? Most channels here need browser cookies and
//! headless auth (Twitter / Reddit / Boss / V2ex / Xueqiu / Xiaoyuzhou /
//! Linkedin / Xhs / Facebook / Instagram). Re-implementing 11 auth flows
//! in pure Rust is a maintenance trap. Agent-Reach wraps them as
//! upstream CLIs (yt-dlp / curl / mcporter / feedparser / ...etc.) with
//! cookies / sessions held in `~/.agent-reach/` (out of this repo).
//!
//! This module is the thin shell-out layer:
//!
//! - [`AgentReachChannel`] — the 16 channels Agent-Reach covers.
//! - [`AgentReachProvider`] — per-channel descriptor (CLI binary,
//!   search / fetch arg templates, output kind, zero-config flag).
//! - [`AGENT_REACH_PROVIDERS`] — `&'static` slice of all 16
//!   descriptors; consistent with the unit-struct `&'static` pattern
//!   used by `web_search` / `wiki` / `github` / `hackernews`.
//! - [`run_upstream`] — async shell-out helper: `which` detection,
//!   30s timeout, proxy env forwarding, stderr capture, friendly error
//!   message.
//! - Per-channel `parse_*` functions for the 5 working channels
//!   (`Search` / `Webpage` / `Rss` / `Youtube` / `Bili`); the other 11
//!   are stubbed — they return `Err("channel X requires CLI not
//!   installed; run `agent-reach install --env=auto --system`")` until
//!   upstream CLIs are available.
//! - [`SearchProvider`] impls for the 5 working channels, adapting
//!   the rich per-channel parser output into [`SearchHit::Web`] with
//!   channel-specific extras packed into the `description` field.
//!
//! ## Security
//!
//! Cookies / tokens for these services live in `~/.agent-reach/`
//! (out of this repo). This crate never persists secrets and never
//! reads `.env` files; configuration is forwarded via `std::env`
//! (e.g. `EXA_API_KEY` for Exa via mcporter).
//!
//! ## Testing
//!
//! The unit tests use `/bin/echo` to exercise the [`run_upstream`]
//! shell-out framework end-to-end without needing any real Agent-Reach
//! CLI installed. Per-channel parsers are tested with fixture
//! strings only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::search_provider::{SearchHit, SearchProvider, SourceKind};

// ────────────────────────────────────────────────────────────────────
// Channel enum — the 16 channels Agent-Reach covers.
// ────────────────────────────────────────────────────────────────────

/// The 16 channels Agent-Reach covers. Names mirror Agent-Reach's
/// `mcporter list` output (kebab-case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentReachChannel {
    /// Exa (via mcporter) — semantic web search.
    Search,
    /// Jina Reader (via curl) — webpage → markdown.
    Webpage,
    /// RSS / Atom feed (via feedparser or inline python).
    Rss,
    /// YouTube search + subtitles (via yt-dlp).
    Youtube,
    /// Bilibili search via bili; video metadata via yt-dlp.
    Bili,
    /// Twitter / X (requires cookies — stubbed).
    Twitter,
    /// GitHub (zero-config — pure-Rust handled in `crate::github`).
    Github,
    /// Reddit (requires cookies — stubbed).
    Reddit,
    /// 小红书 / Xiaohongshu (requires cookies — stubbed).
    Xhs,
    /// LinkedIn (requires cookies — stubbed).
    Linkedin,
    /// BOSS 直聘 (requires cookies — stubbed).
    Boss,
    /// V2EX (zero-config — read-only feed).
    V2ex,
    /// 雪球 / Xueqiu (requires cookies — stubbed).
    Xueqiu,
    /// 小宇宙 / Xiaoyuzhou podcast (zero-config for public feeds).
    Xiaoyuzhou,
    /// Facebook (requires cookies — stubbed).
    Facebook,
    /// Instagram (requires cookies — stubbed).
    Instagram,
}

impl AgentReachChannel {
    /// Canonical channel name (kebab-case, matches Agent-Reach).
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Webpage => "webpage",
            Self::Rss => "rss",
            Self::Youtube => "youtube",
            Self::Bili => "bili",
            Self::Twitter => "twitter",
            Self::Github => "github",
            Self::Reddit => "reddit",
            Self::Xhs => "xhs",
            Self::Linkedin => "linkedin",
            Self::Boss => "boss",
            Self::V2ex => "v2ex",
            Self::Xueqiu => "xueqiu",
            Self::Xiaoyuzhou => "xiaoyuzhou",
            Self::Facebook => "facebook",
            Self::Instagram => "instagram",
        }
    }

    /// Parse a canonical channel name.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "search" => Some(Self::Search),
            "webpage" => Some(Self::Webpage),
            "rss" => Some(Self::Rss),
            "youtube" => Some(Self::Youtube),
            "bili" => Some(Self::Bili),
            "twitter" => Some(Self::Twitter),
            "github" => Some(Self::Github),
            "reddit" => Some(Self::Reddit),
            "xhs" => Some(Self::Xhs),
            "linkedin" => Some(Self::Linkedin),
            "boss" => Some(Self::Boss),
            "v2ex" => Some(Self::V2ex),
            "xueqiu" => Some(Self::Xueqiu),
            "xiaoyuzhou" => Some(Self::Xiaoyuzhou),
            "facebook" => Some(Self::Facebook),
            "instagram" => Some(Self::Instagram),
            _ => None,
        }
    }

    /// True if the channel can be exercised without browser cookies
    /// or platform-specific API keys. Used to decide whether to
    /// auto-fall-back to the channel when no auth is configured.
    pub fn is_zero_config(self) -> bool {
        matches!(
            self,
            Self::Search
                | Self::Webpage
                | Self::Rss
                | Self::Youtube
                | Self::Bili
                | Self::Github
                | Self::V2ex
                | Self::Xiaoyuzhou
        )
    }

    /// Whether this channel is one of the 5 fully-implemented ones
    /// (with parser + `SearchProvider` impl). The other 11 are
    /// stubbed: the descriptor exists but invoking `search()` or
    /// `fetch()` returns an installation hint.
    pub fn is_implemented(self) -> bool {
        matches!(
            self,
            Self::Search | Self::Webpage | Self::Rss | Self::Youtube | Self::Bili
        )
    }

    /// Upstream CLI binary name. For channels whose CLI is not yet
    /// published upstream, the placeholder is `"<missing>"` so that
    /// `run_upstream` clearly errors out.
    pub fn upstream_cli(self) -> &'static str {
        match self {
            Self::Search => "mcporter",
            Self::Webpage => "curl",
            Self::Rss => "python3",
            Self::Youtube => "yt-dlp",
            Self::Bili => "yt-dlp",
            Self::Twitter => "xreach",
            Self::Github => "gh",
            Self::Reddit => "rreach",
            Self::Xhs => "xhsreach",
            Self::Linkedin => "linkedinreach",
            Self::Boss => "bossreach",
            Self::V2ex => "v2exreach",
            Self::Xueqiu => "xueqiureach",
            Self::Xiaoyuzhou => "xiaoyuzhoureach",
            Self::Facebook => "fbreach",
            Self::Instagram => "igreach",
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Provider descriptor + 16 const instances
// ────────────────────────────────────────────────────────────────────

/// How to parse stdout from the upstream CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// Single JSON object.
    Json,
    /// Newline-delimited JSON objects (one per line).
    Jsonl,
    /// Plain text (no parsing).
    Plain,
}

/// Per-channel descriptor: which CLI to invoke, how, and how to
/// interpret its stdout. `&'static` so the consts can live in
/// [`AGENT_REACH_PROVIDERS`] without heap allocations.
#[derive(Debug, Clone, Copy)]
pub struct AgentReachProvider {
    /// Channel this descriptor owns.
    pub channel: AgentReachChannel,
    /// Binary to invoke (e.g. `mcporter`, `curl`, `yt-dlp`).
    pub upstream_cli: &'static str,
    /// Arg prefix for search calls. `None` if the channel cannot
    /// search (it's fetch-only). For templated args use `{query}`
    /// (raw argv, no shell — `tokio::process::Command` quotes for us).
    pub search_args_prefix: Option<&'static [&'static str]>,
    /// Arg prefix for fetch calls (e.g. Jina's `curl -sL https://r.jina.ai/{url}`).
    pub fetch_args_prefix: Option<&'static [&'static str]>,
    /// How stdout is structured.
    pub output_kind: OutputKind,
    /// True if the channel works without browser cookies / API keys.
    pub zero_config: bool,
}

impl AgentReachProvider {
    /// Bilibili search uses its own CLI; video fetch still uses yt-dlp.
    pub fn search_upstream_cli(&self) -> &'static str {
        if self.channel == AgentReachChannel::Bili {
            "bili"
        } else {
            self.upstream_cli
        }
    }
}

/// All 16 [`AgentReachProvider`] descriptors. Static slice — the 5
/// implemented channels each have a parser; the 11 stubbed channels
/// surface a friendly "install upstream CLI" error from `run_upstream`.
///
/// Ordering: matches [`AgentReachChannel`] declaration order (5
/// implemented first, then 11 stubbed).
pub const AGENT_REACH_PROVIDERS: &[AgentReachProvider] = &[
    // ── 5 implemented ────────────────────────────────────────────
    AgentReachProvider {
        channel: AgentReachChannel::Search,
        upstream_cli: "mcporter",
        search_args_prefix: Some(&[
            "call",
            "exa.web_search_exa",
            "--output",
            "json",
            "--no-oauth",
        ]),
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false, // Exa needs API key (via mcporter config)
    },
    AgentReachProvider {
        channel: AgentReachChannel::Webpage,
        upstream_cli: "curl",
        // Read the target directly; HTTP errors must fail the tool call.
        search_args_prefix: None,
        fetch_args_prefix: Some(&["-fsSL"]),
        output_kind: OutputKind::Plain,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Rss,
        upstream_cli: "python3",
        // python3 -c "import feedparser; print(feedparser.parse('<URL>'))"
        search_args_prefix: None,
        fetch_args_prefix: Some(&[]),
        output_kind: OutputKind::Json,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Youtube,
        upstream_cli: "yt-dlp",
        // ytsearch<N>:<Q> --dump-json --skip-download --flat-playlist
        // We invoke with `ytsearch5:rust lang` as a single arg template.
        search_args_prefix: Some(&[
            "--ignore-config",
            "--dump-json",
            "--skip-download",
            "--flat-playlist",
        ]),
        // Metadata only: no media, subtitle files, or cache writes.
        fetch_args_prefix: Some(&[
            "--ignore-config",
            "--dump-single-json",
            "--skip-download",
            "--no-cache-dir",
            "--no-playlist",
        ]),
        output_kind: OutputKind::Jsonl,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Bili,
        upstream_cli: "yt-dlp",
        search_args_prefix: Some(&["search", "--type", "video", "--json"]),
        fetch_args_prefix: Some(&[
            "--ignore-config",
            "--dump-single-json",
            "--skip-download",
            "--no-cache-dir",
            "--no-playlist",
        ]),
        output_kind: OutputKind::Jsonl,
        zero_config: true,
    },
    // ── 11 stubbed ────────────────────────────────────────────────
    AgentReachProvider {
        channel: AgentReachChannel::Twitter,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Github,
        upstream_cli: "gh",
        // `gh search repos <Q>` — zero-config (pure-Rust fallback in
        // crate::github is the primary impl; this is the Agent-Reach
        // shape when the user has `gh` CLI installed).
        search_args_prefix: Some(&["search", "repos"]),
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Reddit,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Xhs,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Linkedin,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Boss,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::V2ex,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Xueqiu,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Xiaoyuzhou,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: true,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Facebook,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
    AgentReachProvider {
        channel: AgentReachChannel::Instagram,
        upstream_cli: "<missing>",
        search_args_prefix: None,
        fetch_args_prefix: None,
        output_kind: OutputKind::Json,
        zero_config: false,
    },
];

/// Look up a [`AgentReachProvider`] by channel. Linear scan over 16
/// entries — fine for hot-path fallback decisions.
pub fn provider_for(channel: AgentReachChannel) -> &'static AgentReachProvider {
    AGENT_REACH_PROVIDERS
        .iter()
        .find(|p| p.channel == channel)
        .expect("every variant has a descriptor in AGENT_REACH_PROVIDERS")
}

// ────────────────────────────────────────────────────────────────────
// Shell-out: which(), build_search_cmd(), build_fetch_cmd(), run_upstream()
// ────────────────────────────────────────────────────────────────────

/// Build the upstream CLI argv for a search call. Returns `None` for
/// fetch-only channels (e.g. Webpage, Rss).
///
/// Build raw argv without shell interpolation. JSON carries Exa arguments;
/// video search explicitly disables downloads and uses the correct platform.
pub fn build_search_argv(
    provider: &AgentReachProvider,
    query: &str,
    limit: usize,
) -> Result<Vec<String>, String> {
    let prefix = provider.search_args_prefix.ok_or_else(|| {
        format!(
            "channel {:?} is fetch-only — it cannot search (query: {:?})",
            provider.channel, query
        )
    })?;
    let mut argv: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    match provider.channel {
        AgentReachChannel::Search => {
            argv.push("--args".into());
            argv.push(
                serde_json::json!({
                    "query": query,
                    "numResults": limit,
                    "objective": "Find relevant public web pages for the query",
                })
                .to_string(),
            );
        }
        AgentReachChannel::Youtube => {
            argv.push("--no-cache-dir".into());
            argv.push(format!("ytsearch{limit}:{query}"));
        }
        AgentReachChannel::Bili => {
            argv.extend(["--max".into(), limit.to_string(), "--".into(), query.into()]);
        }
        _ => argv.push(query.into()),
    }
    Ok(argv)
}

/// Build the upstream CLI argv for a fetch call. Returns `None` for
/// search-only channels (none currently, all support fetch in spirit).
pub fn build_fetch_argv(
    provider: &AgentReachProvider,
    target: &str,
) -> Result<Vec<String>, String> {
    if matches!(provider.channel, AgentReachChannel::Rss) {
        return Ok(vec![
            "-c".into(),
            RSS_FETCH_SCRIPT.into(),
            target.to_string(),
        ]);
    }
    let prefix = provider.fetch_args_prefix.ok_or_else(|| {
        format!(
            "channel {:?} has no fetch_args_prefix; declare one in AGENT_REACH_PROVIDERS",
            provider.channel
        )
    })?;
    let mut argv: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    argv.push(target.to_string());
    Ok(argv)
}

const RSS_FETCH_SCRIPT: &str = r#"import json,sys,urllib.request,xml.etree.ElementTree as ET
url=sys.argv[1]
with urllib.request.urlopen(url, timeout=20) as r:
    data=r.read()
root=ET.fromstring(data)
def local(tag):
    return tag.rsplit('}', 1)[-1]
def child_text(node, names):
    for child in list(node):
        if local(child.tag) in names and child.text:
            return child.text.strip()
    return ""
items=[]
for node in root.iter():
    if local(node.tag) not in ("item", "entry"):
        continue
    link=child_text(node, {"link"})
    if not link:
        for child in list(node):
            if local(child.tag) == "link":
                link=child.attrib.get("href", "")
                if link:
                    break
    items.append({
        "title": child_text(node, {"title"}),
        "url": link,
        "summary": child_text(node, {"description", "summary", "content", "encoded"}),
        "published": child_text(node, {"pubDate", "published", "updated"}),
    })
    if len(items) >= 30:
        break
print(json.dumps({"url": url, "items": items}, ensure_ascii=False))
"#;

/// PATH detection — `tokio::process::Command` does not search `$PATH`
/// for us on Unix when the path is relative, and we want a uniform
/// answer that survives when the upstream is a python module (then
/// we check for the python binary itself).
fn which(cli: &str) -> bool {
    if cli.is_empty() || cli.starts_with('<') {
        return false;
    }
    if let Ok(paths) = std::env::var("PATH") {
        for p in paths.split(':') {
            let full = Path::new(p).join(cli);
            if full.is_file() {
                return true;
            }
        }
    }
    // Common install paths not on `$PATH`.
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for p in [
            home.join(".local/bin").join(cli),
            home.join(".agent-reach-venv/bin").join(cli),
        ] {
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

/// Return whether an upstream CLI is currently available.
pub fn upstream_available(cli: &str) -> bool {
    which(cli)
}

/// Async shell-out to an upstream CLI — used by every Agent-Reach
/// channel. Always passes through `kill_on_drop`, sets a 30s
/// hard timeout, forwards `HTTPS_PROXY` / `HTTP_PROXY` /
/// `ALL_PROXY` / `NO_PROXY` from the calling process, and reports
/// a friendly error on each failure mode.
///
/// Returns stdout as an owned `String` on success. Errors are
/// formatted: "`<cli> not found in PATH ...`", "`<cli> timed out
/// after 30s`", or "`<cli> exited {:?}: <stderr-head-500>`".
pub async fn run_upstream(
    cli: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Result<String, String> {
    if !which(cli) {
        return Err(format!(
            "`{cli}` not found in PATH. Install via \
             `agent-reach install --env=auto --system`. \
             See https://github.com/Panniantong/Agent-Reach"
        ));
    }
    let mut cmd = tokio::process::Command::new(cli);
    cmd.args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        // Force a UTF-8 locale so yt-dlp / curl / python3 don't
        // surprise us with bare bytes in stdout.
        .env("LC_ALL", "C.UTF-8")
        .env("LANG", "C.UTF-8");

    // Forward proxy env vars so the upstream CLI inherits the user's
    // existing proxy config (e.g. CN-side users with `HTTPS_PROXY`).
    for key in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| format!("{cli} timed out after 30s"))?
        .map_err(|e| format!("spawn {cli}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Truncate to 500 chars so the LLM doesn't get a wall of tracebacks.
        let stderr_head: String = if stderr.len() > 500 {
            format!("{}…", &stderr[..500])
        } else {
            stderr.into_owned()
        };
        return Err(format!(
            "{cli} exited {:?}: {}",
            output.status.code(),
            stderr_head
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ────────────────────────────────────────────────────────────────────
// Per-channel parsers — for the 5 implemented channels.
// ────────────────────────────────────────────────────────────────────

/// Accept legacy Exa JSON, MCP structured/text content, and mcporter text.
fn parse_search_exa(stdout: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    fn parse_value(value: &serde_json::Value) -> Result<Vec<SearchHit>, String> {
        if value
            .get("_meta")
            .and_then(|meta| meta.get("ai.exa/rateLimited"))
            .and_then(|flag| flag.as_bool())
            == Some(true)
        {
            return Err("Exa free MCP rate limit reached; wait for quota recovery or configure an eligible Exa endpoint".into());
        }
        if value.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return Err("Exa MCP tool returned isError=true".into());
        }
        if let Some(items) = value.get("results").and_then(|v| v.as_array()) {
            return items
                .iter()
                .map(|item| {
                    web_hit(
                        item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                        item.get("text")
                            .or_else(|| item.get("snippet"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    )
                })
                .collect();
        }
        if let Some(structured) = value.get("structuredContent") {
            if structured.get("results").is_some() {
                return parse_value(structured);
            }
        }
        if let Some(blocks) = value.get("content").and_then(|v| v.as_array()) {
            let mut hits = Vec::new();
            let mut recognized = false;
            for block in blocks {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    hits.extend(parse_search_exa(text, usize::MAX)?);
                    recognized = true;
                }
            }
            return if recognized {
                Ok(hits)
            } else {
                Err("Exa response contains no searchable text content".into())
            };
        }
        if let Some(text) = value.as_str() {
            return parse_search_exa(text, usize::MAX);
        }
        Err("Unrecognized Exa response schema (expected results or MCP content)".into())
    }

    let mut hits = if let Ok(value) = serde_json::from_str(stdout) {
        parse_value(&value)?
    } else {
        // The hosted Exa MCP returns records headed by Title: and URL:.
        let mut hits = Vec::new();
        let mut title: Option<&str> = None;
        let mut target = "";
        let mut description = Vec::new();
        for line in stdout.lines() {
            if let Some(next_title) = line.strip_prefix("Title: ") {
                if let Some(title) = title {
                    hits.push(web_hit(title, target, &description.join("\n"))?);
                }
                title = Some(next_title.trim());
                target = "";
                description.clear();
            } else if let Some(url) = line.strip_prefix("URL: ") {
                target = url.trim();
            } else if title.is_some() {
                description.push(line);
            }
        }
        if let Some(title) = title {
            hits.push(web_hit(title, target, &description.join("\n"))?);
        }
        if hits.is_empty() {
            return Err("Unrecognized or empty Exa search response".into());
        }
        hits
    };
    hits.truncate(limit);
    for (idx, hit) in hits.iter_mut().enumerate() {
        if let SearchHit::Web { position, .. } = hit {
            *position = idx + 1;
        }
    }
    Ok(hits)
}

fn web_hit(title: &str, target: &str, description: &str) -> Result<SearchHit, String> {
    let url = url::Url::parse(target).map_err(|_| "Search result is missing a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Search result URL must use HTTP(S)".into());
    }
    Ok(SearchHit::Web {
        title: title.into(),
        url: target.into(),
        description: description.into(),
        position: 0,
    })
}

fn parse_bili_search(stdout: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|_| "Invalid JSON from bili search")?;
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err("bili search returned ok=false or an unrecognized response".into());
    }
    let items = value
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("bili search response is missing data[]")?;
    items
        .iter()
        .take(limit)
        .enumerate()
        .map(|(idx, item)| {
            let bvid = item
                .get("bvid")
                .and_then(|v| v.as_str())
                .filter(|id| id.starts_with("BV") && id.chars().all(|c| c.is_ascii_alphanumeric()))
                .ok_or("bili video result is missing a valid bvid")?;
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .ok_or("bili video result is missing title")?;
            let mut hit = web_hit(
                title,
                &format!("https://www.bilibili.com/video/{bvid}"),
                &format!(
                    "author={}; views={}; duration={}",
                    item.get("author").and_then(|v| v.as_str()).unwrap_or(""),
                    item.get("play").unwrap_or(&serde_json::Value::Null),
                    item.get("duration").and_then(|v| v.as_str()).unwrap_or("")
                ),
            )?;
            if let SearchHit::Web { position, .. } = &mut hit {
                *position = idx + 1;
            }
            Ok(hit)
        })
        .collect()
}

/// Parse yt-dlp `--dump-json --flat-playlist` output (JSONL — one
/// JSON object per line, when invoked as `ytsearch<N>:<query>`).
fn parse_youtube_dump_json(stdout: &str, limit: usize) -> Vec<SearchHit> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .enumerate()
        .filter_map(|(i, line)| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let title = v.get("title").and_then(|x| x.as_str())?.to_string();
            let url = v
                .get("webpage_url")
                .or_else(|| v.get("url"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let channel = v
                .get("channel")
                .or_else(|| v.get("uploader"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let duration = v.get("duration").and_then(|x| x.as_u64());
            let views = v.get("view_count").and_then(|x| x.as_u64());
            let published = v
                .get("upload_date")
                .and_then(|x| x.as_str())
                .map(String::from);

            // Pack channel-specific extras into the description so
            // the LLM sees them. Format:  "<extras>".
            let mut extras_parts: Vec<String> = Vec::new();
            if !channel.is_empty() {
                extras_parts.push(format!("channel={channel}"));
            }
            if let Some(d) = duration {
                extras_parts.push(format!("duration_secs={d}"));
            }
            if let Some(v) = views {
                extras_parts.push(format!("views={v}"));
            }
            if let Some(p) = &published {
                extras_parts.push(format!("upload_date={p}"));
            }
            let description = extras_parts.join(" · ");

            Some(SearchHit::Web {
                title,
                url,
                description,
                position: i + 1,
            })
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────
// SearchProvider impls — 3 channels exposed as SearchProvider
// ────────────────────────────────────────────────────────────────────
//
// Of the 5 implemented channels, 3 are search-shaped and expose a
// `SearchProvider::search()`:
//
//   - [`AgentReachSearchProvider`]   — Exa via mcporter
//   - [`AgentReachYoutubeProvider`]  — yt-dlp `ytsearch<N>:<Q>`
//   - [`AgentReachBiliProvider`]     — yt-dlp bili extractor
//
// The other two (Webpage / Rss) are fetch-only and exposed via the
// top-level [`fetch_channel`] function.
//
// Why not `AgentReachWebpageProvider` / `AgentReachRssProvider`?
// Because Jina Reader is fetch-only (you give it a URL, it returns
// markdown), and feedparser parses one feed at a time. Both have
// arity-1 signatures, not "search a list of queries", so wrapping
// them in `SearchProvider` would lie about their shape.

/// Exa (via mcporter) — semantic web search.
pub struct AgentReachSearchProvider;

#[async_trait]
impl SearchProvider for AgentReachSearchProvider {
    fn name(&self) -> &'static str {
        "agent-reach:search"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let provider = provider_for(AgentReachChannel::Search);
        let argv = build_search_argv(provider, query, limit)?;
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let stdout = run_upstream(provider.upstream_cli, &argv_refs, &[]).await?;
        parse_search_exa(&stdout, limit)
    }
}

/// YouTube (via yt-dlp).
pub struct AgentReachYoutubeProvider;

#[async_trait]
impl SearchProvider for AgentReachYoutubeProvider {
    fn name(&self) -> &'static str {
        "agent-reach:youtube"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let provider = provider_for(AgentReachChannel::Youtube);
        let argv = build_search_argv(provider, query, limit)?;
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let stdout = run_upstream(provider.upstream_cli, &argv_refs, &[]).await?;
        Ok(parse_youtube_dump_json(&stdout, limit))
    }
}

/// Bilibili search via the bili CLI's read-only video search command.
pub struct AgentReachBiliProvider;

#[async_trait]
impl SearchProvider for AgentReachBiliProvider {
    fn name(&self) -> &'static str {
        "agent-reach:bili"
    }
    fn kind(&self) -> SourceKind {
        SourceKind::Web
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        let provider = provider_for(AgentReachChannel::Bili);
        let argv = build_search_argv(provider, query, limit)?;
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let stdout = run_upstream(provider.search_upstream_cli(), &argv_refs, &[]).await?;
        parse_bili_search(&stdout, limit)
    }
}

// ────────────────────────────────────────────────────────────────────
// Top-level fetch_channel() — for fetch-only channels
// ────────────────────────────────────────────────────────────────────

/// Fetch a single URL / feed / video through the channel's upstream
/// CLI. Returns HTML for Webpage, JSON for Rss, and selected JSON metadata
/// for Youtube/Bili (no transcripts or subtitles). Returns `Err` for
/// stubbed channels with an install hint.
pub async fn fetch_channel(channel: AgentReachChannel, target: &str) -> Result<String, String> {
    let provider = provider_for(channel);
    if provider.upstream_cli == "<missing>" {
        return Err(format!(
            "channel {:?} is stubbed; install via \
             `agent-reach install --env=auto --system`",
            channel
        ));
    }
    let argv = build_fetch_argv(provider, target)?;
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = run_upstream(provider.upstream_cli, &argv_refs, &[]).await?;
    validate_fetch_output(channel, &output)?;
    if matches!(
        channel,
        AgentReachChannel::Youtube | AgentReachChannel::Bili
    ) {
        video_metadata(&output)
    } else {
        Ok(output)
    }
}

fn video_metadata(output: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|_| "Invalid video metadata JSON from yt-dlp")?;
    if value
        .get("title")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err("Video metadata contains no title".into());
    }
    // Exclude signed media URLs and HTTP headers from the tool response.
    let mut metadata = serde_json::Map::new();
    for key in [
        "id",
        "title",
        "webpage_url",
        "description",
        "duration",
        "uploader",
        "channel",
        "upload_date",
        "view_count",
    ] {
        if let Some(value) = value.get(key) {
            metadata.insert(key.into(), value.clone());
        }
    }
    Ok(serde_json::Value::Object(metadata).to_string())
}

pub(crate) fn validate_fetch_output(
    channel: AgentReachChannel,
    output: &str,
) -> Result<(), String> {
    if output.trim().is_empty() {
        return Err(format!("Channel '{}' returned no content", channel.name()));
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────
// Tests — exercise the shell-out framework with /bin/echo, plus
//          parser fixtures.
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_name_matches_kebab_case() {
        assert_eq!(AgentReachChannel::Search.name(), "search");
        assert_eq!(AgentReachChannel::Webpage.name(), "webpage");
        assert_eq!(AgentReachChannel::Github.name(), "github");
        assert_eq!(AgentReachChannel::Xiaoyuzhou.name(), "xiaoyuzhou");
        assert_eq!(
            AgentReachChannel::parse("youtube"),
            Some(AgentReachChannel::Youtube)
        );
        assert_eq!(AgentReachChannel::parse("not-a-channel"), None);
    }

    #[test]
    fn providers_have_all_sixteen_channels() {
        assert_eq!(AGENT_REACH_PROVIDERS.len(), 16);
        for &ch in &[
            AgentReachChannel::Search,
            AgentReachChannel::Webpage,
            AgentReachChannel::Rss,
            AgentReachChannel::Youtube,
            AgentReachChannel::Bili,
            AgentReachChannel::Twitter,
            AgentReachChannel::Github,
            AgentReachChannel::Reddit,
            AgentReachChannel::Xhs,
            AgentReachChannel::Linkedin,
            AgentReachChannel::Boss,
            AgentReachChannel::V2ex,
            AgentReachChannel::Xueqiu,
            AgentReachChannel::Xiaoyuzhou,
            AgentReachChannel::Facebook,
            AgentReachChannel::Instagram,
        ] {
            assert_eq!(provider_for(ch).channel, ch);
        }
    }

    #[test]
    fn five_channels_marked_implemented() {
        let implemented: Vec<AgentReachChannel> = AGENT_REACH_PROVIDERS
            .iter()
            .filter(|p| p.channel.is_implemented())
            .map(|p| p.channel)
            .collect();
        assert_eq!(
            implemented,
            vec![
                AgentReachChannel::Search,
                AgentReachChannel::Webpage,
                AgentReachChannel::Rss,
                AgentReachChannel::Youtube,
                AgentReachChannel::Bili,
            ]
        );
    }

    #[test]
    fn which_returns_false_for_missing_cli() {
        assert!(!which("definitely-not-a-real-cli-xyz-12345"));
    }

    #[test]
    fn which_returns_true_for_echo() {
        // /bin/echo is on every Unix system that has a shell.
        assert!(which("echo"));
    }

    #[tokio::test]
    async fn run_upstream_echo_succeeds() {
        let out = run_upstream("/bin/echo", &["hello", "agent-reach"], &[])
            .await
            .expect("echo should succeed");
        assert_eq!(out.trim(), "hello agent-reach");
    }

    #[tokio::test]
    async fn run_upstream_missing_cli_returns_install_hint() {
        let r = run_upstream("definitely-not-a-real-cli-xyz", &[], &[]).await;
        let err = r.expect_err("should fail when CLI is missing");
        assert!(err.contains("not found in PATH"), "got: {err}");
        assert!(err.contains("agent-reach install"), "got: {err}");
    }

    #[tokio::test]
    async fn run_upstream_exits_with_code_on_failure() {
        // `false` exits 1 on every Unix.
        let r = run_upstream("/usr/bin/false", &[], &[]).await;
        let err = r.expect_err("false should fail");
        assert!(err.contains("exited"), "got: {err}");
        assert!(err.contains('1'), "got: {err}");
    }

    #[tokio::test]
    async fn search_provider_for_stubbed_channel_errors_with_install_hint() {
        let provider = AgentReachChannel::Twitter;
        let err = fetch_channel(provider, "https://example.com")
            .await
            .expect_err("stubbed channel should error");
        assert!(err.contains("stubbed"), "got: {err}");
        assert!(err.contains("agent-reach install"), "got: {err}");
    }

    #[test]
    fn parse_search_exa_extracts_results() {
        let stdout = r#"{
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "text": "A language."},
                {"title": "Tokio", "url": "https://tokio.rs", "snippet": "Async runtime."}
            ]
        }"#;
        let hits = parse_search_exa(stdout, 10).unwrap();
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web { title, url, .. } => {
                assert_eq!(title, "Rust");
                assert_eq!(url, "https://rust-lang.org");
            }
            other => panic!("expected Web hit, got {other:?}"),
        }
    }

    #[test]
    fn parse_search_exa_handles_empty_results() {
        let hits = parse_search_exa(r#"{"results": []}"#, 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_search_exa_handles_missing_results_key() {
        assert!(parse_search_exa(r#"{}"#, 10).is_err());
    }

    #[test]
    fn exa_argv_preserves_query_as_json_and_uses_real_tool() {
        let query = "quote \" and newline\n$(never-a-shell) --args";
        let argv = build_search_argv(provider_for(AgentReachChannel::Search), query, 2).unwrap();
        assert_eq!(
            &argv[..5],
            &[
                "call",
                "exa.web_search_exa",
                "--output",
                "json",
                "--no-oauth"
            ]
        );
        assert_eq!(argv[5], "--args");
        let args: serde_json::Value = serde_json::from_str(&argv[6]).unwrap();
        assert_eq!(args["query"], query);
        assert_eq!(args["numResults"], 2);
        assert!(!args["objective"].as_str().unwrap().is_empty());
    }

    #[test]
    fn exa_mcp_text_and_structured_responses_are_compatible() {
        let records = "Title: Rust\nURL: https://rust-lang.org/\nHighlights:\nFirst\n\nTitle: Tokio\nURL: https://tokio.rs/\nHighlights:\nSecond";
        for stdout in [records.to_string(), serde_json::json!({
            "content": [{"type":"text", "text":records}]
        }).to_string(), serde_json::json!({"content": [{"type":"text", "text":
            "{\"results\":[{\"title\":\"Rust\",\"url\":\"https://rust-lang.org/\",\"text\":\"First\"}]}"
        }]}).to_string(), serde_json::json!({"structuredContent":{"results":[
            {"title":"Rust","url":"https://rust-lang.org/","snippet":"First"}
        ]}}).to_string()] {
            let hits = parse_search_exa(&stdout, 1).unwrap();
            assert_eq!(hits.len(), 1);
            match &hits[0] {
                SearchHit::Web { title, url, description, position } => {
                    assert_eq!(title, "Rust");
                    assert_eq!(url, "https://rust-lang.org/");
                    assert!(description.contains("First"));
                    assert_eq!(*position, 1);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let hits = parse_search_exa(records, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(matches!(&hits[1], SearchHit::Web { position: 2, .. }));
    }

    #[test]
    fn exa_errors_and_malformed_responses_are_not_empty_successes() {
        let rate_limited = serde_json::json!({
            "_meta": {"ai.exa/rateLimited": true},
            "content": [{"type": "text", "text": "You've hit Exa's free MCP rate limit."}]
        });
        assert!(parse_search_exa(&rate_limited.to_string(), 1)
            .unwrap_err()
            .contains("rate limit"));
        for stdout in [
            "",
            "upstream unavailable",
            "{}",
            r#"{"isError":true,"content":[]}"#,
            r#"{"results":[{"title":"missing URL"}]}"#,
            "Title: Invalid\nURL: file:///secret",
        ] {
            assert!(parse_search_exa(stdout, 1).is_err(), "{stdout}");
        }
    }

    #[test]
    fn video_search_argv_is_read_only_and_platform_specific() {
        let query = "--not-an-option";
        let youtube =
            build_search_argv(provider_for(AgentReachChannel::Youtube), query, 1).unwrap();
        for flag in [
            "--ignore-config",
            "--dump-json",
            "--skip-download",
            "--flat-playlist",
            "--no-cache-dir",
        ] {
            assert!(youtube.iter().any(|arg| arg == flag));
        }
        assert_eq!(youtube.last().unwrap(), "ytsearch1:--not-an-option");
        let bili = provider_for(AgentReachChannel::Bili);
        assert_eq!(bili.search_upstream_cli(), "bili");
        assert_eq!(
            build_search_argv(bili, query, 2).unwrap(),
            ["search", "--type", "video", "--json", "--max", "2", "--", query]
        );
    }

    #[test]
    fn bili_search_parses_real_cli_schema_and_rejects_failures() {
        let stdout = r#"{"ok":true,"schema_version":"1","data":[
            {"bvid":"BV1hp4y1k7SV","title":"Rust tutorial","author":"Author","play":42,"duration":"10:00"},
            {"bvid":"BV1xx411c7mD","title":"Second"}]}"#;
        let hits = parse_bili_search(stdout, 1).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHit::Web {
                title,
                url,
                description,
                position,
            } => {
                assert_eq!(title, "Rust tutorial");
                assert_eq!(url, "https://www.bilibili.com/video/BV1hp4y1k7SV");
                assert!(description.contains("views=42"));
                assert_eq!(*position, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(parse_bili_search(r#"{"ok":true,"data":[]}"#, 1)
            .unwrap()
            .is_empty());
        for stdout in [
            "not json",
            r#"{"ok":false,"data":[]}"#,
            r#"{"ok":true}"#,
            r#"{"ok":true,"data":[{"title":"no id"}]}"#,
        ] {
            assert!(parse_bili_search(stdout, 1).is_err());
        }
    }

    #[test]
    fn empty_fetch_is_an_error() {
        for channel in [
            AgentReachChannel::Youtube,
            AgentReachChannel::Bili,
            AgentReachChannel::Webpage,
            AgentReachChannel::Rss,
        ] {
            for output in ["", " \n\t"] {
                let error = validate_fetch_output(channel, output).unwrap_err();
                assert!(error.contains("no content"));
            }
            assert!(validate_fetch_output(channel, "actual content").is_ok());
        }
    }

    #[test]
    fn video_fetch_requests_metadata_without_media_or_subtitle_writes() {
        for channel in [AgentReachChannel::Youtube, AgentReachChannel::Bili] {
            let argv =
                build_fetch_argv(provider_for(channel), "https://example.com/video").unwrap();
            assert_eq!(
                argv,
                [
                    "--ignore-config",
                    "--dump-single-json",
                    "--skip-download",
                    "--no-cache-dir",
                    "--no-playlist",
                    "https://example.com/video"
                ]
            );
        }
        let metadata: serde_json::Value = serde_json::from_str(&video_metadata(r#"{
            "id":"BV1xx411c7mD","title":"Video without subtitles","webpage_url":"https://www.bilibili.com/video/BV1xx411c7mD",
            "subtitles":{},"http_headers":{"Cookie":"private"},"url":"https://media.example/?token=private"
        }"#).unwrap()).unwrap();
        assert_eq!(metadata["title"], "Video without subtitles");
        assert!(metadata.get("http_headers").is_none());
        assert!(metadata.get("url").is_none());
        for invalid in ["", "null", "{}", r#"{"title":"  "}"#] {
            assert!(video_metadata(invalid).is_err());
        }
    }

    #[test]
    fn webpage_fetch_rejects_http_errors() {
        assert_eq!(
            build_fetch_argv(
                provider_for(AgentReachChannel::Webpage),
                "https://example.com"
            )
            .unwrap(),
            ["-fsSL", "https://example.com"]
        );
    }

    #[test]
    fn parse_youtube_extracts_metadata() {
        let stdout = r#"{"title": "Rust in 100 Seconds", "webpage_url": "https://youtu.be/x", "channel": "Fireship", "duration": 162, "view_count": 1234567, "upload_date": "20240101"}
{"title": "Another", "webpage_url": "https://youtu.be/y", "channel": "Other", "duration": 60, "view_count": 100, "upload_date": "20250101"}"#;
        let hits = parse_youtube_dump_json(stdout, 10);
        assert_eq!(hits.len(), 2);
        match &hits[0] {
            SearchHit::Web {
                title,
                description,
                position,
                ..
            } => {
                assert_eq!(title, "Rust in 100 Seconds");
                assert_eq!(*position, 1);
                assert!(description.contains("channel=Fireship"));
                assert!(description.contains("duration_secs=162"));
                assert!(description.contains("views=1234567"));
            }
            other => panic!("expected Web hit, got {other:?}"),
        }
    }

    #[test]
    fn parse_youtube_skips_empty_lines_and_garbage() {
        let stdout = "\nnot-json-line\n{\"title\": \"ok\", \"webpage_url\": \"https://x\"}\n";
        let hits = parse_youtube_dump_json(stdout, 10);
        assert_eq!(hits.len(), 1, "should skip blank + garbage lines");
    }

    #[test]
    fn parse_youtube_respects_limit() {
        let stdout = (0..20)
            .map(|i| format!(r#"{{"title": "t{i}", "webpage_url": "https://x/{i}"}}"#))
            .collect::<Vec<_>>()
            .join("\n");
        let hits = parse_youtube_dump_json(&stdout, 3);
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn fetch_channel_search_only_returns_error() {
        // Search (Exa) is search-only — no fetch_args_prefix.
        let r = fetch_channel(AgentReachChannel::Search, "rust")
            .await
            .expect_err("search-only should error");
        assert!(
            r.contains("no fetch_args_prefix") || r.contains("stubbed"),
            "got: {r}"
        );
    }

    #[test]
    fn rss_fetch_argv_uses_inline_python() {
        let provider = provider_for(AgentReachChannel::Rss);
        let argv = build_fetch_argv(provider, "https://example.com/feed.xml").unwrap();
        assert_eq!(argv[0], "-c");
        assert!(argv[1].contains("urllib.request"));
        assert_eq!(argv[2], "https://example.com/feed.xml");
    }
}
