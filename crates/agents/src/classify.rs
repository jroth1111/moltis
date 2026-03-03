//! Shared provider error classification and retry-hint parsing.
//!
//! This module is the single source of truth for provider error kinds and
//! routing semantics used by both the agent runner and provider failover chain.

/// Maximum retry-after hint accepted by default (24 hours).
pub const DEFAULT_RETRY_AFTER_MAX_MS: u64 = 86_400_000;

/// Routing decision derived from a provider error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorRoutingAction {
    /// Retry on the same provider.
    Retry,
    /// Fail over to another provider/key.
    Failover,
    /// Compact context and retry.
    CompactAndRetry,
    /// Surface directly to the user.
    SurfaceToUser,
}

/// How a provider error should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// 429 — rotate to next provider.
    RateLimit,
    /// 401/403 — rotate (bad key or permissions).
    AuthError,
    /// 5xx — retry/failover depending on caller policy.
    ServerError,
    /// Network/operation timeout.
    Timeout,
    /// Billing/usage limit exhausted — rotate.
    BillingExhausted,
    /// Plan/model access error: rotate to another provider.
    NonRetryableRateLimit,
    /// Context window exceeded — caller should compact.
    ContextWindow,
    /// 400-level request shape error — surface directly.
    InvalidRequest,
    /// Unrecognised error.
    Unknown,
}

impl ProviderErrorKind {
    /// Whether this error kind is safe to retry in some form.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self.routing_action(),
            ErrorRoutingAction::Retry
                | ErrorRoutingAction::Failover
                | ErrorRoutingAction::CompactAndRetry
        )
    }

    /// Deterministic routing action for this error class.
    #[must_use]
    pub fn routing_action(self) -> ErrorRoutingAction {
        match self {
            Self::ServerError | Self::Timeout => ErrorRoutingAction::Retry,
            Self::RateLimit
            | Self::AuthError
            | Self::BillingExhausted
            | Self::NonRetryableRateLimit
            | Self::Unknown => ErrorRoutingAction::Failover,
            Self::ContextWindow => ErrorRoutingAction::CompactAndRetry,
            Self::InvalidRequest => ErrorRoutingAction::SurfaceToUser,
        }
    }

    /// Whether this error kind should trigger provider failover.
    #[must_use]
    pub fn should_failover(self) -> bool {
        matches!(self.routing_action(), ErrorRoutingAction::Failover)
    }
}

const CONTEXT_WINDOW_PATTERNS: &[&str] = &[
    "context_length_exceeded",
    "max_tokens",
    "too many tokens",
    "request too large",
    "maximum context length",
    "context window",
    "token limit",
    "content_too_large",
    "request_too_large",
];

const TIMEOUT_PATTERNS: &[&str] = &[
    "timed out",
    "timeout",
    "deadline exceeded",
    "operation took too long",
    "request took too long",
];

const PLAN_LIMIT_PATTERNS: &[&str] = &[
    "your plan does not include",
    "model not available",
    "not available on your plan",
    "upgrade your plan",
    "organization does not have access",
];

const RATE_LIMIT_PATTERNS: &[&str] = &["429", "rate limit", "rate_limit", "too many requests"];

const AUTH_PATTERNS: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "forbidden",
    "invalid api key",
    "invalid_api_key",
    "authentication",
];

const BILLING_PATTERNS: &[&str] = &[
    "billing",
    "quota",
    "insufficient_quota",
    "usage limit",
    "credit",
];

const SERVER_PATTERNS: &[&str] = &[
    "500",
    "502",
    "503",
    "504",
    "529",
    "internal server error",
    "bad gateway",
    "service unavailable",
    "overloaded",
    "server_error",
    "the server had an error processing your request",
];

/// Classify an `anyhow::Error` into [`ProviderErrorKind`].
#[must_use]
pub fn classify_error(err: &anyhow::Error) -> ProviderErrorKind {
    classify_error_message(&err.to_string())
}

/// Classify an error message into [`ProviderErrorKind`].
#[must_use]
pub fn classify_error_message(msg: &str) -> ProviderErrorKind {
    let msg = msg.to_ascii_lowercase();

    // Context window first, because it overlaps with generic "request too large".
    if CONTEXT_WINDOW_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::ContextWindow;
    }

    if TIMEOUT_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::Timeout;
    }

    if PLAN_LIMIT_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::NonRetryableRateLimit;
    }

    if BILLING_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::BillingExhausted;
    }

    if RATE_LIMIT_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::RateLimit;
    }

    if AUTH_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::AuthError;
    }

    if SERVER_PATTERNS.iter().any(|p| msg.contains(p)) {
        return ProviderErrorKind::ServerError;
    }

    if msg.contains("400") || msg.contains("bad request") || msg.contains("invalid_request") {
        return ProviderErrorKind::InvalidRequest;
    }

    ProviderErrorKind::Unknown
}

/// Parse a retry-after duration (in milliseconds) from a fragment.
///
/// The fragment should start at or before the first numeric token.
#[must_use]
pub fn parse_retry_delay_ms_from_fragment(
    fragment: &str,
    unit_default_ms: bool,
    max_ms: u64,
) -> Option<u64> {
    let start = fragment.find(|c: char| c.is_ascii_digit())?;
    let tail = &fragment[start..];

    let numeric_len = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .count();
    if numeric_len == 0 {
        return None;
    }

    let amount = tail[..numeric_len].parse::<f64>().ok()?;
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }

    let unit = tail[numeric_len..].trim_start();
    let ms = if unit.starts_with("ms") || unit.starts_with("millisecond") {
        amount
    } else if unit.starts_with("sec") || unit.starts_with("second") || unit.starts_with('s') {
        amount * 1_000.0
    } else if unit.starts_with("min") || unit.starts_with("minute") || unit.starts_with('m') {
        amount * 60_000.0
    } else if unit_default_ms {
        amount
    } else {
        amount * 1_000.0
    };

    Some((ms.round() as u64).clamp(1, max_ms))
}

/// Extract retry delay hints embedded in provider error text.
///
/// Supports patterns like:
/// - `retry_after_ms=1234`
/// - `Retry-After: 30`
/// - `retry after 30s`
/// - `retry in 45 seconds`
#[must_use]
pub fn extract_retry_after_ms(msg: &str, max_ms: u64) -> Option<u64> {
    let lower = msg.to_ascii_lowercase();
    for (needle, default_ms) in [
        ("retry_after_ms=", true),
        ("retry-after-ms=", true),
        ("retry_after=", false),
        ("retry-after:", false),
        ("retry after ", false),
        ("retry in ", false),
    ] {
        if let Some(idx) = lower.find(needle) {
            let fragment = &lower[idx + needle.len()..];
            if let Some(ms) = parse_retry_delay_ms_from_fragment(fragment, default_ms, max_ms) {
                return Some(ms);
            }
        }
    }
    None
}

/// Parse retry-after hints with the default max window.
#[must_use]
pub fn parse_retry_after_ms(msg: &str) -> Option<u64> {
    extract_retry_after_ms(msg, DEFAULT_RETRY_AFTER_MAX_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_timeout() {
        assert_eq!(
            classify_error_message("request timed out after 30s"),
            ProviderErrorKind::Timeout
        );
    }

    #[test]
    fn classifies_context_window_before_request_too_large() {
        assert_eq!(
            classify_error_message("request too large: context window exceeded"),
            ProviderErrorKind::ContextWindow
        );
    }

    #[test]
    fn classifies_non_retryable_rate_limit_before_rate_limit() {
        assert_eq!(
            classify_error_message(
                "Your plan does not include access to this model and hit rate limit"
            ),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(
            extract_retry_after_ms("Retry-After: 15", 60_000),
            Some(15_000)
        );
    }

    #[test]
    fn parses_retry_after_decimal_seconds() {
        assert_eq!(
            extract_retry_after_ms("retry after 5.5 seconds", 60_000),
            Some(5_500)
        );
    }

    #[test]
    fn routing_actions_are_deterministic() {
        assert_eq!(
            ProviderErrorKind::ServerError.routing_action(),
            ErrorRoutingAction::Retry
        );
        assert_eq!(
            ProviderErrorKind::RateLimit.routing_action(),
            ErrorRoutingAction::Failover
        );
        assert_eq!(
            ProviderErrorKind::ContextWindow.routing_action(),
            ErrorRoutingAction::CompactAndRetry
        );
        assert_eq!(
            ProviderErrorKind::InvalidRequest.routing_action(),
            ErrorRoutingAction::SurfaceToUser
        );
    }

    #[test]
    fn retry_actions_do_not_trigger_failover() {
        assert!(!ProviderErrorKind::ServerError.should_failover());
        assert!(!ProviderErrorKind::Timeout.should_failover());
        assert!(ProviderErrorKind::RateLimit.should_failover());
    }
}
