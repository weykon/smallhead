//! Shared HTTP client builder for all search providers.
//!
//! Centralises `reqwest::Client` construction so every provider gets the
//! same proxy / user-agent / timeout configuration. Direct usage is rare —
//! most providers call `build_client` once per request.

use std::time::Duration;

/// Default per-request timeout for search providers.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Build a `reqwest::Client` configured for search traffic.
///
/// Honours `HTTPS_PROXY` / `HTTP_PROXY` env vars via a per-request
/// [`reqwest::Proxy::custom`] closure. Sets a [`DEFAULT_TIMEOUT_SECS`]
/// ceiling; callers may override via `timeout(...)` on the returned builder
/// if they need a longer window.
pub(crate) fn build_client(user_agent: &str) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .proxy(reqwest::Proxy::custom(|url| {
            // Read per-scheme proxy env var. ALL_PROXY is the fallback.
            let key = match url.scheme() {
                "https" => "HTTPS_PROXY",
                "http" => "HTTP_PROXY",
                _ => "ALL_PROXY",
            };
            std::env::var(key).ok()
        }))
        .build()
        .map_err(|e| format!("build client: {e}"))
}
