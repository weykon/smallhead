//! `smallhead-tools` — external-information tools for the agent runtime.
//!
//! This crate implements the **provider-side** of the `web_search` tool:
//! HTTP client + HTML utilities + health-aware fallback chains, plus the
//! concrete pure-Rust search providers (Mojeek / DuckDuckGo / Bing.cn /
//! Bing.com / Baidu / Wikipedia / GitHub / HackerNews / Brave).
//!
//! The [`WebSearchTool`](smallhead_core::WebSearchTool) type is the
//! unified `ToolImpl` entry point registered
//! into [`ToolKind`](smallhead_core::ToolKind) by the host crate
//! (`smallhead-cli` / `smallhead-tui` / `smallhead-server`).
//!
//! ## Module layout
//!
//! - [`http_client`] — shared `reqwest::Client` builder with proxy + UA + timeout
//! - [`html_utils`] — `strip_html` / `collapse_whitespace` (and friends)
//! - `provider_health` — health registry + EWMA decay + skip policy
//! - [`search_provider`] — `SearchHit` enum + `SearchProvider` trait + CJK-aware fallback chain
//! - [`web_search`] — 5 web source providers (CJK-aware chain)
//! - [`wiki`] — Wikipedia (zh / en)
//! - [`github`] — GitHub repos search
//! - [`hackernews`] — HackerNews (Algolia)
//! - [`brave`] — Brave Search (optional `BRAVE_API_KEY`)
//! - [`agent_reach`] — 16 Agent-Reach channels via shell-out (yt-dlp /
//!   curl / mcporter / …); bridge for Twitter / Reddit / Boss / V2ex
//!   / Xueqiu / Xiaoyuzhou / Linkedin / Xhs / Facebook / Instagram
//!   that need browser cookies or platform-specific API keys.
//! - [`agent_reach_tool`] — unified `AgentReachTool` CLI entry point
//! - [`web_search_tool`] — unified `WebSearchTool: ToolImpl`
//!
//! ## Security
//!
//! Cookies / tokens for upstream services live in `~/.agent-reach/` (out of
//! this repository). This crate never persists secrets; `BRAVE_API_KEY` is
//! read at runtime from `std::env`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent_reach;
pub mod agent_reach_tool;
pub mod brave;
pub mod github;
pub mod hackernews;
pub mod html_utils;
pub mod http_client;
pub mod search_provider;
pub mod web_search;
pub mod web_search_tool;
pub mod wiki;

mod provider_health;
