//! Permission errors.

use thiserror::Error;

/// Permission error returned when a tool input fails a policy check.
#[derive(Debug, Error)]
pub enum PermissionError {
    /// Command matches a blacklisted dangerous pattern.
    ///
    /// `pattern` is the literal substring from `DANGEROUS_PATTERNS` that
    /// matched; `command` is the original command string the caller supplied.
    #[error("blacklisted pattern '{pattern}' in command: {command}")]
    BlacklistedPattern {
        /// The dangerous pattern that matched (literal substring).
        pattern: &'static str,
        /// The original command string that was rejected.
        command: String,
    },
}
