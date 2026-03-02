//! Unified LLM provider error classification.
//!
//! Single source of truth for mapping provider HTTP status codes and error
//! body text into actionable error kinds with routing decisions.

/// How a provider error should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// 429 — rotate to next provider.
    RateLimit,
    /// 401/403 — rotate (bad key or permissions).
    AuthError,
    /// 5xx — rotate to next provider.
    ServerError,
    /// Request timed out — rotate to next provider.
    Timeout,
    /// Billing/usage limit exhausted — rotate.
    BillingExhausted,
    /// Plan/model does not support this request — different from billing exhaustion.
    /// Example: "Your plan doesn't include access to this model."
    /// Should skip the entire provider, not just the key.
    NonRetryableRateLimit,
    /// Context window exceeded — don't rotate, caller should compact.
    ContextWindow,
    /// 400, bad format — don't rotate, it'll fail everywhere.
    InvalidRequest,
    /// Unrecognised error — attempt failover.
    Unknown,
}

/// What the caller should do in response to a classified error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingAction {
    /// Retry with the same or next provider after a delay.
    Retry,
    /// Skip to the next provider in the chain.
    Failover,
    /// The context is too large — compact the conversation and retry.
    CompactAndRetry,
    /// Terminal error — surface directly to the user.
    SurfaceToUser,
}

impl ProviderErrorKind {
    /// Whether this error kind should trigger failover to the next provider.
    #[must_use]
    pub fn should_failover(self) -> bool {
        matches!(
            self,
            Self::RateLimit
                | Self::AuthError
                | Self::ServerError
                | Self::Timeout
                | Self::BillingExhausted
                | Self::NonRetryableRateLimit
                | Self::Unknown
        )
    }

    /// Whether this error is worth retrying (possibly after a delay).
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::ServerError | Self::Timeout | Self::Unknown
        )
    }

    /// Determine the routing action for this error kind.
    #[must_use]
    pub fn routing_action(self) -> RoutingAction {
        match self {
            Self::RateLimit | Self::ServerError | Self::Timeout => RoutingAction::Retry,
            Self::AuthError
            | Self::BillingExhausted
            | Self::NonRetryableRateLimit
            | Self::Unknown => RoutingAction::Failover,
            Self::ContextWindow => RoutingAction::CompactAndRetry,
            Self::InvalidRequest => RoutingAction::SurfaceToUser,
        }
    }
}

// ── Pattern arrays ──────────────────────────────────────────────────────

/// Error patterns that indicate the context window has been exceeded.
pub const CONTEXT_WINDOW_PATTERNS: &[&str] = &[
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

/// Error patterns that indicate a transient server error worth retrying.
///
/// Reconciled from runner.rs and provider_chain.rs — includes all known
/// patterns from both: `504` (from provider_chain), `http 529`/
/// `"the server had an error processing your request"` (from runner.rs).
pub const RETRYABLE_SERVER_PATTERNS: &[&str] = &[
    "500",
    "502",
    "503",
    "504",
    "http 529",
    "server_error",
    "internal server error",
    "overloaded",
    "bad gateway",
    "service unavailable",
    "the server had an error processing your request",
];

/// Error patterns that indicate provider-side rate limiting.
pub const RATE_LIMIT_PATTERNS: &[&str] = &[
    "429",
    "status=429",
    "status 429",
    "status: 429",
    "too many requests",
    "rate limit",
    "rate_limit",
];

/// Error patterns that indicate the account is out of credits/quota.
pub const BILLING_QUOTA_PATTERNS: &[&str] = &[
    "billing",
    "quota",
    "insufficient_quota",
    "usage limit",
    "credit",
    "billing details",
    "billing limit",
    "credit balance",
];

/// Error patterns that indicate an authentication or authorization failure.
pub const AUTH_PATTERNS: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "forbidden",
    "invalid api key",
    "invalid_api_key",
    "authentication",
];

/// Plan/model access error patterns — non-retryable rate limits.
const NON_RETRYABLE_PATTERNS: &[&str] = &[
    "your plan does not include",
    "model not available",
    "not available on your plan",
    "upgrade your plan",
    "organization does not have access",
];

/// Timeout-related error patterns.
const TIMEOUT_PATTERNS: &[&str] = &[
    "timed out",
    "timeout",
    "request timeout",
    "deadline exceeded",
    "gateway timeout",
];

// ── Classification ──────────────────────────────────────────────────────

/// Classify a provider error into a `ProviderErrorKind` based on an
/// optional HTTP status code and the error body/message text.
#[must_use]
pub fn classify_error(status: Option<u16>, body: &str) -> ProviderErrorKind {
    let lower = body.to_lowercase();

    // Context window — must check first since "request too large" overlaps.
    if CONTEXT_WINDOW_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::ContextWindow;
    }

    // Plan/model access errors — non-retryable with key rotation.
    // Must check before the general rate limit check.
    if NON_RETRYABLE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::NonRetryableRateLimit;
    }

    // Billing / quota exhaustion — check before rate limit since messages
    // often contain both "429" and "insufficient_quota"; billing is the
    // more specific and non-retryable diagnosis.
    if BILLING_QUOTA_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::BillingExhausted;
    }

    // Rate limiting.
    if status == Some(429) || RATE_LIMIT_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::RateLimit;
    }

    // Auth errors.
    if matches!(status, Some(401) | Some(403)) || AUTH_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::AuthError;
    }

    // Timeout.
    if status == Some(408) || TIMEOUT_PATTERNS.iter().any(|p| lower.contains(p)) {
        return ProviderErrorKind::Timeout;
    }

    // Server errors.
    if matches!(status, Some(500..=599))
        || RETRYABLE_SERVER_PATTERNS.iter().any(|p| lower.contains(p))
    {
        return ProviderErrorKind::ServerError;
    }

    // Invalid request (400-level, non-auth, non-rate-limit).
    if status == Some(400) || lower.contains("bad request") || lower.contains("invalid_request") {
        return ProviderErrorKind::InvalidRequest;
    }

    ProviderErrorKind::Unknown
}

/// Convenience wrapper: classify from an `anyhow::Error` (no status code).
///
/// This preserves the original API used by `provider_chain.rs`.
#[must_use]
pub fn classify_anyhow(err: &anyhow::Error) -> ProviderErrorKind {
    classify_error(None, &err.to_string())
}

// ── Retry-After parsing ─────────────────────────────────────────────────

/// Parse a retry-after duration (in milliseconds) from an error message.
/// Looks for patterns like "retry after 30", "retry-after: 5.5", "Retry-After: 60".
#[must_use]
pub fn parse_retry_after_ms(msg: &str) -> Option<u64> {
    let re = regex::Regex::new(r"(?i)retry.?after[:\s]+(\d+\.?\d*)").ok()?;
    let cap = re.captures(msg)?;
    let secs: f64 = cap.get(1)?.as_str().parse().ok()?;
    Some((secs * 1000.0) as u64)
}

/// Parse a retry delay (in ms) from a fragment like `"1234ms"`, `"30s"`,
/// `"2 minutes"`.
///
/// `unit_default_ms`: when `true`, bare numbers (no unit suffix) are
/// interpreted as milliseconds; otherwise as seconds.
///
/// The result is clamped to `[1, max_ms]`.
pub fn parse_retry_delay_ms_from_fragment(
    fragment: &str,
    unit_default_ms: bool,
    max_ms: u64,
) -> Option<u64> {
    let start = fragment.find(|c: char| c.is_ascii_digit())?;
    let tail = &fragment[start..];
    let digits_len = tail.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    let amount = tail[..digits_len].parse::<u64>().ok()?;
    let unit = tail[digits_len..].trim_start();

    let ms = if unit.starts_with("ms") || unit.starts_with("millisecond") {
        amount
    } else if unit.starts_with("sec") || unit.starts_with("second") || unit.starts_with('s') {
        amount.saturating_mul(1_000)
    } else if unit.starts_with("min") || unit.starts_with("minute") || unit.starts_with('m') {
        amount.saturating_mul(60_000)
    } else if unit_default_ms {
        amount
    } else {
        amount.saturating_mul(1_000)
    };

    Some(ms.clamp(1, max_ms))
}

/// Extract retry delay hints embedded in provider error messages.
///
/// Supports patterns like:
/// - `retry_after_ms=1234`
/// - `Retry-After: 30`
/// - `retry after 30s`
/// - `retry in 45 seconds`
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

// ── Tests ───────────────────────────────────────────────────────────────

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_error ──────────────────────────────────────────────

    #[test]
    fn classify_context_window_by_body() {
        assert_eq!(
            classify_error(
                None,
                "context_length_exceeded: maximum context length is 200000"
            ),
            ProviderErrorKind::ContextWindow
        );
        assert_eq!(
            classify_error(None, "too many tokens in this request"),
            ProviderErrorKind::ContextWindow
        );
        assert_eq!(
            classify_error(None, "content_too_large"),
            ProviderErrorKind::ContextWindow
        );
    }

    #[test]
    fn classify_rate_limit_by_status() {
        assert_eq!(classify_error(Some(429), ""), ProviderErrorKind::RateLimit);
    }

    #[test]
    fn classify_rate_limit_by_body() {
        assert_eq!(
            classify_error(None, "429 Too Many Requests: rate limit exceeded"),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            classify_error(None, "rate_limit_exceeded"),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            classify_error(None, "HTTP 429 Too Many Requests"),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            classify_error(None, "status=429 upstream limit"),
            ProviderErrorKind::RateLimit
        );
    }

    #[test]
    fn classify_auth_by_status() {
        assert_eq!(classify_error(Some(401), ""), ProviderErrorKind::AuthError);
        assert_eq!(classify_error(Some(403), ""), ProviderErrorKind::AuthError);
    }

    #[test]
    fn classify_auth_by_body() {
        assert_eq!(
            classify_error(None, "401 Unauthorized"),
            ProviderErrorKind::AuthError
        );
        assert_eq!(
            classify_error(None, "invalid api key provided"),
            ProviderErrorKind::AuthError
        );
    }

    #[test]
    fn classify_billing_by_body() {
        assert_eq!(
            classify_error(None, "insufficient_quota: billing limit reached"),
            ProviderErrorKind::BillingExhausted
        );
        assert_eq!(
            classify_error(
                None,
                "You exceeded your current quota, please check your plan and billing details."
            ),
            ProviderErrorKind::BillingExhausted
        );
    }

    #[test]
    fn classify_server_error_by_status() {
        assert_eq!(
            classify_error(Some(500), ""),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(Some(502), ""),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(Some(503), ""),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(Some(504), ""),
            ProviderErrorKind::ServerError
        );
    }

    #[test]
    fn classify_server_error_by_body() {
        assert_eq!(
            classify_error(None, "502 Bad Gateway"),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(None, "overloaded_error: server is overloaded"),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(None, "The server had an error processing your request."),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            classify_error(None, "HTTP 529 site overloaded"),
            ProviderErrorKind::ServerError
        );
    }

    #[test]
    fn classify_timeout_by_status() {
        assert_eq!(classify_error(Some(408), ""), ProviderErrorKind::Timeout);
    }

    #[test]
    fn classify_timeout_by_body() {
        assert_eq!(
            classify_error(None, "request timed out"),
            ProviderErrorKind::Timeout
        );
        assert_eq!(
            classify_error(None, "deadline exceeded waiting for response"),
            ProviderErrorKind::Timeout
        );
        assert_eq!(
            classify_error(None, "gateway timeout"),
            ProviderErrorKind::Timeout
        );
    }

    #[test]
    fn classify_invalid_request_by_status() {
        assert_eq!(
            classify_error(Some(400), ""),
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn classify_invalid_request_by_body() {
        assert_eq!(
            classify_error(None, "400 Bad Request: invalid JSON"),
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn classify_non_retryable_rate_limit() {
        assert_eq!(
            classify_error(None, "Your plan does not include access to this model"),
            ProviderErrorKind::NonRetryableRateLimit
        );
        assert_eq!(
            classify_error(None, "model not available for your account"),
            ProviderErrorKind::NonRetryableRateLimit
        );
        assert_eq!(
            classify_error(None, "Please upgrade your plan to access this feature"),
            ProviderErrorKind::NonRetryableRateLimit
        );
        assert_eq!(
            classify_error(None, "Your organization does not have access to this model"),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_error(None, "connection reset by peer"),
            ProviderErrorKind::Unknown
        );
    }

    #[test]
    fn classify_anyhow_wrapper() {
        let err = anyhow::anyhow!("429 rate limit exceeded");
        assert_eq!(classify_anyhow(&err), ProviderErrorKind::RateLimit);
    }

    // ── routing_action ──────────────────────────────────────────────

    #[test]
    fn routing_action_retry_variants() {
        assert_eq!(
            ProviderErrorKind::RateLimit.routing_action(),
            RoutingAction::Retry
        );
        assert_eq!(
            ProviderErrorKind::ServerError.routing_action(),
            RoutingAction::Retry
        );
        assert_eq!(
            ProviderErrorKind::Timeout.routing_action(),
            RoutingAction::Retry
        );
    }

    #[test]
    fn routing_action_failover_variants() {
        assert_eq!(
            ProviderErrorKind::AuthError.routing_action(),
            RoutingAction::Failover
        );
        assert_eq!(
            ProviderErrorKind::BillingExhausted.routing_action(),
            RoutingAction::Failover
        );
        assert_eq!(
            ProviderErrorKind::NonRetryableRateLimit.routing_action(),
            RoutingAction::Failover
        );
        assert_eq!(
            ProviderErrorKind::Unknown.routing_action(),
            RoutingAction::Failover
        );
    }

    #[test]
    fn routing_action_compact_and_retry() {
        assert_eq!(
            ProviderErrorKind::ContextWindow.routing_action(),
            RoutingAction::CompactAndRetry
        );
    }

    #[test]
    fn routing_action_surface_to_user() {
        assert_eq!(
            ProviderErrorKind::InvalidRequest.routing_action(),
            RoutingAction::SurfaceToUser
        );
    }

    // ── is_retryable ────────────────────────────────────────────────

    #[test]
    fn is_retryable_true_variants() {
        assert!(ProviderErrorKind::RateLimit.is_retryable());
        assert!(ProviderErrorKind::ServerError.is_retryable());
        assert!(ProviderErrorKind::Timeout.is_retryable());
        assert!(ProviderErrorKind::Unknown.is_retryable());
    }

    #[test]
    fn is_retryable_false_variants() {
        assert!(!ProviderErrorKind::AuthError.is_retryable());
        assert!(!ProviderErrorKind::BillingExhausted.is_retryable());
        assert!(!ProviderErrorKind::NonRetryableRateLimit.is_retryable());
        assert!(!ProviderErrorKind::ContextWindow.is_retryable());
        assert!(!ProviderErrorKind::InvalidRequest.is_retryable());
    }

    // ── should_failover ─────────────────────────────────────────────

    #[test]
    fn should_failover_mapping() {
        assert!(ProviderErrorKind::RateLimit.should_failover());
        assert!(ProviderErrorKind::AuthError.should_failover());
        assert!(ProviderErrorKind::ServerError.should_failover());
        assert!(ProviderErrorKind::Timeout.should_failover());
        assert!(ProviderErrorKind::BillingExhausted.should_failover());
        assert!(ProviderErrorKind::NonRetryableRateLimit.should_failover());
        assert!(ProviderErrorKind::Unknown.should_failover());
        assert!(!ProviderErrorKind::ContextWindow.should_failover());
        assert!(!ProviderErrorKind::InvalidRequest.should_failover());
    }

    // ── Retry-after parsing ─────────────────────────────────────────

    #[test]
    fn parse_retry_after_ms_seconds() {
        assert_eq!(
            parse_retry_after_ms("rate limited, retry after 30"),
            Some(30_000)
        );
        assert_eq!(parse_retry_after_ms("Retry-After: 5.5"), Some(5_500));
        assert_eq!(parse_retry_after_ms("no hint here"), None);
    }

    #[test]
    fn parse_retry_after_ms_integer() {
        assert_eq!(parse_retry_after_ms("retry-after: 60"), Some(60_000));
    }

    #[test]
    fn extract_retry_after_ms_variants() {
        assert_eq!(
            extract_retry_after_ms("Anthropic API error (retry_after_ms=1234)", 60_000),
            Some(1234)
        );
        assert_eq!(
            extract_retry_after_ms("HTTP 429 Retry-After: 15", 60_000),
            Some(15_000)
        );
        assert_eq!(
            extract_retry_after_ms("rate limit exceeded, retry in 7 seconds", 60_000),
            Some(7_000)
        );
    }

    #[test]
    fn parse_retry_delay_ms_from_fragment_units() {
        assert_eq!(
            parse_retry_delay_ms_from_fragment("1234ms", true, 60_000),
            Some(1234)
        );
        assert_eq!(
            parse_retry_delay_ms_from_fragment("30s", false, 60_000),
            Some(30_000)
        );
        assert_eq!(
            parse_retry_delay_ms_from_fragment("2 minutes", false, 60_000),
            Some(60_000)
        );
        assert_eq!(
            parse_retry_delay_ms_from_fragment("500", true, 60_000),
            Some(500)
        );
        assert_eq!(
            parse_retry_delay_ms_from_fragment("500", false, 60_000),
            Some(60_000)
        );
    }

    // ── Context window check priority ───────────────────────────────

    #[test]
    fn context_window_takes_priority_over_request_too_large() {
        // "request too large" could match InvalidRequest but context window
        // patterns are checked first.
        assert_eq!(
            classify_error(Some(400), "request too large for context window"),
            ProviderErrorKind::ContextWindow
        );
    }
}
