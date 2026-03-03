//! Request trace identifier for correlating logs across service boundaries.

/// A unique identifier for a single request, threaded through the entire
/// call chain (gateway ingress → runner → tools → hook payloads).
///
/// Represented as a UUID v4 string for easy logging and serialization.
/// All fields are `Option<String>` so callers that don't have a trace context
/// can pass `None` without breaking backward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraceId(pub String);

impl TraceId {
    /// Generate a new random trace ID (UUID v4).
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// View the raw string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<TraceId> for String {
    fn from(t: TraceId) -> Self {
        t.0
    }
}
