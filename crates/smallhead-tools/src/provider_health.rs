//! Provider health tracking — lets the `web_search` fallback chain remember
//! which sources have recently died.
//!
//! ## Why
//!
//! The chain is serial; each dead source costs the full 15s timeout. For
//! CJK users that hurts worst — if 3 of 4 sources are dead, we burn 60s
//! returning 0 results.
//!
//! This module maintains a per-provider health snapshot:
//! - success → score rises
//! - failure → score drops, `last_error_kind` recorded
//! - old events (>1h) decay automatically; recovered sources rebound
//!
//! `run_fallback_chain` consults `should_skip` before hitting the network
//! and skips sources whose recent history is bad.
//!
//! ## Scope
//!
//! - Covers only the fallback chain (5 web providers). Wiki / Brave /
//!   GitHub / HN are direct calls — no chain — so they're not filtered
//!   here (independent experiment).
//! - No parallel fan-out (architectural change, post-spike).
//! - No caching (independent experiment).
//!
//! ## Usage
//!
//! ```ignore
//! use crate::provider_health::{global, classify_error};
//!
//! // chain entry filter:
//! if global().lock().unwrap().should_skip("mojeek") {
//!     eprintln!("[web_search] skipping mojeek (recent failures)");
//!     continue;
//! }
//!
//! // chain exit record:
//! match try_provider(...).await {
//!     Ok(hits) if !hits.is_empty() => {
//!         global().lock().unwrap().record_success("mojeek");
//!         return Ok(hits);
//!     }
//!     Ok(_) => global().lock().unwrap().record_success("mojeek"), // 0 hits still success
//!     Err(e) => {
//!         global().lock().unwrap().record_failure("mojeek", classify_error(&e));
//!         last_err = Some(e);
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Failure classification — keeps `should_skip` precise.
///
/// `CjkRejected` is fine to skip forever; `Timeout` may be a transient
/// network blip, so let it decay and try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// Network timeout (including transport timeouts).
    Timeout,
    /// HTTP status not in 2xx.
    HttpStatus(u16),
    /// Detected captcha / "are you human".
    Captcha,
    /// Server actively rejects CJK (e.g. Mojeek 403 + follow-up captcha).
    CjkRejected,
    /// HTML / JSON parse failure (structure changed).
    ParseError,
    /// TLS handshake failure / cert issue.
    Tls,
    /// Unknown error.
    Unknown,
}

impl ProviderErrorKind {
    /// Short label, e.g. `"timeout"` / `"cjk-rejected"`.
    ///
    /// Note: `HttpStatus(_)` is *not* `&'static str` since it carries the
    /// code; this method returns a static category label only.
    pub fn label(self) -> &'static str {
        match self {
            ProviderErrorKind::Timeout => "timeout",
            ProviderErrorKind::HttpStatus(_) => "http-status",
            ProviderErrorKind::Captcha => "captcha",
            ProviderErrorKind::CjkRejected => "cjk-rejected",
            ProviderErrorKind::ParseError => "parse-error",
            ProviderErrorKind::Tls => "tls",
            ProviderErrorKind::Unknown => "unknown",
        }
    }
}

/// Per-provider health snapshot.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    /// Composite score `[0.0, 1.0]`, time-decayed.
    pub score: f32,
    /// Consecutive failure count (resets on success).
    pub consecutive_failures: u32,
    /// Total successes (pre-decay).
    #[allow(dead_code)]
    pub total_success: u32,
    /// Total failures (pre-decay).
    #[allow(dead_code)]
    pub total_failure: u32,
    /// Most recent error.
    pub last_error: Option<(ProviderErrorKind, u64)>,
}

#[derive(Debug, Clone)]
struct ProviderHealth {
    success_count: u32,
    failure_count: u32,
    last_error_kind: Option<ProviderErrorKind>,
    last_error_at: u64,
    last_success_at: u64,
    consecutive_failures: u32,
}

impl ProviderHealth {
    fn new() -> Self {
        Self {
            success_count: 0,
            failure_count: 0,
            last_error_kind: None,
            last_error_at: 0,
            last_success_at: 0,
            consecutive_failures: 0,
        }
    }

    fn record_success(&mut self, now: u64) {
        self.success_count = self.success_count.saturating_add(1);
        self.consecutive_failures = 0;
        self.last_success_at = now;
    }

    fn record_failure(&mut self, kind: ProviderErrorKind, now: u64) {
        self.failure_count = self.failure_count.saturating_add(1);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error_kind = Some(kind);
        self.last_error_at = now;
    }

    /// Decayed effective counts: events >1h old weight 0.5× per hour,
    /// events >2h effectively zero. EWMA-lite, good enough for skip policy.
    fn decayed_counts(&self, now: u64) -> (f32, f32) {
        const HALF_LIFE_SECS: u64 = 3600; // 1h half-life
        let succ_weight = if self.success_count == 0 {
            1.0
        } else {
            let elapsed = now.saturating_sub(self.last_success_at);
            0.5_f32.powf(elapsed as f32 / HALF_LIFE_SECS as f32)
        };
        let fail_weight = if self.failure_count == 0 {
            1.0
        } else {
            let elapsed = now.saturating_sub(self.last_error_at);
            0.5_f32.powf(elapsed as f32 / HALF_LIFE_SECS as f32)
        };
        (
            self.success_count as f32 * succ_weight,
            self.failure_count as f32 * fail_weight,
        )
    }

    fn score(&self, now: u64) -> f32 {
        let (succ, fail) = self.decayed_counts(now);
        // +1 smoothing to avoid 0/0 boundary
        succ / (succ + fail + 1.0)
    }

    /// Default threshold: score < 0.3 → skip.
    /// Belt-and-braces with `consecutive_failures >= 3`:
    /// - low score: long-term unreliable
    /// - many consecutive: recently broken
    fn should_skip(&self, now: u64) -> bool {
        if self.consecutive_failures >= 3 && self.score(now) < 0.3 {
            return true;
        }
        // CjkRejected is a hard error: recognised once, skip for 2h.
        if matches!(self.last_error_kind, Some(ProviderErrorKind::CjkRejected))
            && now.saturating_sub(self.last_error_at) < 7200
        {
            return true;
        }
        false
    }
}

/// Global health registry — shared across all sources.
#[derive(Debug, Default)]
pub struct ProviderHealthRegistry {
    map: HashMap<String, ProviderHealth>,
}

impl ProviderHealthRegistry {
    /// Empty registry — use [`global`](global) for the shared instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful call.
    pub fn record_success(&mut self, provider: &str) {
        let now = now_secs();
        self.map
            .entry(provider.to_string())
            .or_insert_with(ProviderHealth::new)
            .record_success(now);
    }

    /// Record a failed call with classification.
    pub fn record_failure(&mut self, provider: &str, kind: ProviderErrorKind) {
        let now = now_secs();
        self.map
            .entry(provider.to_string())
            .or_insert_with(ProviderHealth::new)
            .record_failure(kind, now);
    }

    /// Snapshot for one provider (defaults to healthy if unseen).
    pub fn health(&self, provider: &str) -> HealthSnapshot {
        let now = now_secs();
        match self.map.get(provider) {
            Some(h) => HealthSnapshot {
                score: h.score(now),
                consecutive_failures: h.consecutive_failures,
                total_success: h.success_count,
                total_failure: h.failure_count,
                last_error: h.last_error_kind.map(|k| (k, h.last_error_at)),
            },
            None => HealthSnapshot {
                score: 1.0,
                consecutive_failures: 0,
                total_success: 0,
                total_failure: 0,
                last_error: None,
            },
        }
    }

    /// Should the chain skip this provider?
    pub fn should_skip(&self, provider: &str) -> bool {
        let now = now_secs();
        self.map
            .get(provider)
            .map(|h| h.should_skip(now))
            .unwrap_or(false)
    }

    /// Snapshot of all providers (sorted by name) — diagnostic only.
    #[allow(dead_code)]
    pub fn snapshot_all(&self) -> Vec<(String, HealthSnapshot)> {
        let now = now_secs();
        let mut out: Vec<_> = self
            .map
            .iter()
            .map(|(k, h)| {
                (
                    k.clone(),
                    HealthSnapshot {
                        score: h.score(now),
                        consecutive_failures: h.consecutive_failures,
                        total_success: h.success_count,
                        total_failure: h.failure_count,
                        last_error: h.last_error_kind.map(|k| (k, h.last_error_at)),
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Clear all state — for tests.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.map.clear();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Classify an error string into a [`ProviderErrorKind`].
///
/// All search providers surface errors as `String`, so we infer the cause
/// from text keywords.
pub fn classify_error(err: &str) -> ProviderErrorKind {
    let lower = err.to_lowercase();

    // Order matters: captcha beats cjk (both can mention the keyword).
    if lower.contains("captcha")
        || lower.contains("are you human")
        || lower.contains("bot detection")
    {
        return ProviderErrorKind::Captcha;
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return ProviderErrorKind::Timeout;
    }
    if lower.contains("tls") || lower.contains("handshake") || lower.contains("certificate") {
        return ProviderErrorKind::Tls;
    }
    if lower.contains("parse") || lower.contains("json") {
        return ProviderErrorKind::ParseError;
    }
    // Mojeek's CJK rejection returns 403 + follow-up captcha, caught above.
    // But bare 403 is also a likely CJK rejection.
    if let Some(code) = extract_status_code(err) {
        if code == 403 || code == 451 {
            return ProviderErrorKind::CjkRejected;
        }
        return ProviderErrorKind::HttpStatus(code);
    }
    if lower.contains("error sending request")
        || lower.contains("connection refused")
        || lower.contains("dns")
        || lower.contains("connect")
        || lower.contains("proxy")
        || lower.contains("network")
    {
        return ProviderErrorKind::Timeout;
    }
    ProviderErrorKind::Unknown
}

fn extract_status_code(s: &str) -> Option<u16> {
    // reqwest Display formats:
    //   old: "status code 403" / "error status: 403 Forbidden"
    //   new: "status: HTTP status client error (403 Forbidden) for url (...)"
    // Recognise both.
    // 1) `(403 Forbidden)` — new format
    if let Some(open) = s.find('(') {
        let after = &s[open + 1..];
        if let Some(space) = after.find(' ') {
            if let Ok(code) = after[..space].parse::<u16>() {
                return Some(code);
            }
        }
    }
    // 2) `status: 403` / `status code 403` — old format
    let idx = s.find("status")?;
    let after = &s[idx + 6..];
    let trimmed = after.trim_start_matches(|c: char| !c.is_ascii_digit() && c != ':');
    let trimmed = trimmed.trim_start_matches(':').trim_start();
    let mut end = 0;
    for (i, c) in trimmed.char_indices() {
        if c.is_ascii_digit() {
            end = i + 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    trimmed[..end].parse().ok()
}

/// Shared, process-local health registry handle.
pub type SharedHealthRegistry = Arc<Mutex<ProviderHealthRegistry>>;

/// Global health registry — process-local, never persisted.
pub fn global() -> SharedHealthRegistry {
    static REG: OnceLock<SharedHealthRegistry> = OnceLock::new();
    REG.get_or_init(|| Arc::new(Mutex::new(ProviderHealthRegistry::new())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_provider_is_healthy() {
        let r = ProviderHealthRegistry::new();
        let h = r.health("never-seen");
        assert_eq!(h.score, 1.0);
        assert!(!r.should_skip("never-seen"));
    }

    #[test]
    fn successes_keep_score_high() {
        let mut r = ProviderHealthRegistry::new();
        for _ in 0..10 {
            r.record_success("ok-src");
        }
        let h = r.health("ok-src");
        assert!(h.score > 0.9, "expected high score, got {}", h.score);
        assert!(!r.should_skip("ok-src"));
    }

    #[test]
    fn consecutive_failures_drop_score_and_trigger_skip() {
        let mut r = ProviderHealthRegistry::new();
        for _ in 0..3 {
            r.record_failure("bad-src", ProviderErrorKind::Captcha);
        }
        let h = r.health("bad-src");
        assert_eq!(h.consecutive_failures, 3);
        assert!(h.score < 0.3, "expected low score, got {}", h.score);
        assert!(
            r.should_skip("bad-src"),
            "should skip after 3 consecutive failures"
        );
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let mut r = ProviderHealthRegistry::new();
        r.record_failure("x", ProviderErrorKind::Timeout);
        r.record_failure("x", ProviderErrorKind::Timeout);
        r.record_success("x");
        let h = r.health("x");
        assert_eq!(h.consecutive_failures, 0);
    }

    #[test]
    fn mixed_history_lets_provider_try() {
        let mut r = ProviderHealthRegistry::new();
        r.record_success("a");
        r.record_success("a");
        r.record_failure("a", ProviderErrorKind::Timeout);
        r.record_success("a");
        r.record_success("a");
        // 2 failures but 8 successes — score should be ok, consecutive = 0
        let h = r.health("a");
        assert!(!r.should_skip("a"));
        assert_eq!(h.consecutive_failures, 0);
    }

    #[test]
    fn cjk_rejected_hard_skip_within_2h() {
        let mut r = ProviderHealthRegistry::new();
        r.record_failure("mojeek", ProviderErrorKind::CjkRejected);
        let h = r.health("mojeek");
        assert!(h.last_error.is_some());
        assert!(r.should_skip("mojeek"));
    }

    #[test]
    fn classifies_timeout() {
        assert_eq!(
            classify_error("request timeout exceeded"),
            ProviderErrorKind::Timeout
        );
        assert_eq!(
            classify_error("operation timed out"),
            ProviderErrorKind::Timeout
        );
        assert_eq!(
            classify_error("error sending request"),
            ProviderErrorKind::Timeout
        );
    }

    #[test]
    fn classifies_captcha() {
        assert_eq!(
            classify_error("captcha challenge detected"),
            ProviderErrorKind::Captcha
        );
        assert_eq!(classify_error("are you human?"), ProviderErrorKind::Captcha);
    }

    #[test]
    fn classifies_http_status_403_as_cjk() {
        // Old reqwest format
        assert_eq!(
            classify_error("status: 403 Forbidden"),
            ProviderErrorKind::CjkRejected
        );
        // New reqwest format: status code in parens
        assert_eq!(
            classify_error(
                "status: HTTP status client error (403 Forbidden) for url (https://example.com)"
            ),
            ProviderErrorKind::CjkRejected
        );
        assert_eq!(
            classify_error(
                "error sending request: status: HTTP status server error (503 Service Unavailable)"
            ),
            ProviderErrorKind::HttpStatus(503)
        );
    }

    #[test]
    fn classifies_http_status_500() {
        assert_eq!(
            classify_error("status: 500 Internal Server Error"),
            ProviderErrorKind::HttpStatus(500)
        );
    }

    #[test]
    fn classifies_tls_error() {
        assert_eq!(
            classify_error("TLS handshake failed"),
            ProviderErrorKind::Tls
        );
    }

    #[test]
    fn classifies_parse_error() {
        assert_eq!(
            classify_error("parse json error"),
            ProviderErrorKind::ParseError
        );
    }

    #[test]
    fn classifies_unknown() {
        assert_eq!(
            classify_error("weird thing happened"),
            ProviderErrorKind::Unknown
        );
    }

    #[test]
    fn snapshot_all_sorts_by_name() {
        let mut r = ProviderHealthRegistry::new();
        r.record_success("zzz");
        r.record_success("aaa");
        let snap = r.snapshot_all();
        assert_eq!(snap[0].0, "aaa");
        assert_eq!(snap[1].0, "zzz");
    }
}
