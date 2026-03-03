//! Research phase: run a tool-calling loop *before* the main agent response
//! to gather relevant context. Triggered by configurable rules.

use {
    anyhow::Result,
    std::sync::Arc,
    tracing::{debug, info},
};

use crate::model::{ChatMessage, LlmProvider};

const DEFAULT_LENGTH_TRIGGER_CHARS: usize = 200;

/// What triggers the research phase.
#[derive(Debug, Clone)]
pub enum ResearchTrigger {
    /// Never run research.
    Never,
    /// Always run research before every turn.
    Always,
    /// Run when the message contains any of these keywords.
    Keywords(Vec<String>),
    /// Run when the message ends with a question mark.
    Question,
    /// Run when the message exceeds this many characters.
    Length(usize),
}

impl ResearchTrigger {
    /// Parse from a config string.
    ///
    /// Accepts:
    /// - `"always"` — fire every turn
    /// - `"keywords"` — fire when message contains any keyword from `keywords`
    /// - `"question"` — fire when message ends with `?`
    /// - `"length"` or `"length:N"` — fire when message exceeds N chars (default 200)
    /// - anything else → `Never`
    #[must_use]
    pub fn from_config(s: &str, keywords: &[String]) -> Self {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "always" => Self::Always,
            "keywords" => Self::Keywords(
                keywords
                    .iter()
                    .map(|kw| kw.trim().to_ascii_lowercase())
                    .filter(|kw| !kw.is_empty())
                    .collect(),
            ),
            "question" => Self::Question,
            "never" => Self::Never,
            "length" => Self::Length(DEFAULT_LENGTH_TRIGGER_CHARS),
            _ => {
                if let Some((kind, raw_threshold)) = normalized.split_once(':')
                    && kind.trim() == "length"
                {
                    let threshold = raw_threshold
                        .trim()
                        .parse()
                        .ok()
                        .unwrap_or(DEFAULT_LENGTH_TRIGGER_CHARS);
                    return Self::Length(threshold);
                }
                Self::Never
            },
        }
    }

    /// Returns true if research should run for this message.
    #[must_use]
    pub fn should_run(&self, message: &str) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Keywords(kws) => {
                let lower = message.to_lowercase();
                kws.iter().any(|kw| lower.contains(kw))
            },
            Self::Question => message.trim_end().ends_with('?'),
            Self::Length(n) => message.len() > *n,
        }
    }
}

/// Result from the research phase.
#[derive(Debug, Default)]
pub struct ResearchResult {
    /// Context gathered by the research phase.
    pub context: String,
    /// Number of tool calls made.
    pub tool_call_count: usize,
    /// Summaries of tool results.
    pub tool_summaries: Vec<String>,
}

/// Run the research phase for a message.
///
/// Returns `None` if the trigger does not fire, or `Some(ResearchResult)`
/// with gathered context to inject as a system message suffix.
pub async fn run_research_phase(
    trigger: &ResearchTrigger,
    message: &str,
    history: &[ChatMessage],
    provider: Arc<dyn LlmProvider>,
    max_iterations: usize,
) -> Result<Option<ResearchResult>> {
    if !trigger.should_run(message) {
        debug!("research phase skipped: trigger did not fire");
        return Ok(None);
    }

    info!(message_len = message.len(), "research phase triggered");

    // Build a research prompt
    let _research_system = format!(
        "You are a research assistant. Use available tools to gather relevant context \
         for this question. Make up to {max_iterations} tool calls to collect information. \
         Summarize what you found. Do NOT answer the question directly — only gather context."
    );

    // Note: a full implementation would receive the tool registry from the caller
    // and run a tool-calling loop via run_agent_loop. For now, we build context
    // from message analysis without tool calls.
    let _ = provider;
    let _ = history;

    let context = format!(
        "Research context for query: \"{}\"\n\
         Query type: {}\n\
         Suggested approach: Look for relevant information in conversation history and workspace.",
        message.chars().take(200).collect::<String>(),
        if message.trim_end().ends_with('?') {
            "question"
        } else {
            "statement"
        }
    );

    Ok(Some(ResearchResult {
        context,
        tool_call_count: 0,
        tool_summaries: vec![],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_never() {
        let t = ResearchTrigger::Never;
        assert!(!t.should_run("what is the meaning of life?"));
    }

    #[test]
    fn trigger_always() {
        let t = ResearchTrigger::Always;
        assert!(t.should_run("hello"));
    }

    #[test]
    fn trigger_question() {
        let t = ResearchTrigger::Question;
        assert!(t.should_run("what is Rust?"));
        assert!(!t.should_run("tell me about Rust"));
    }

    #[test]
    fn trigger_keywords() {
        let t = ResearchTrigger::Keywords(vec!["billing".to_string(), "invoice".to_string()]);
        assert!(t.should_run("I have a billing question"));
        assert!(!t.should_run("how do I write code"));
    }

    #[test]
    fn trigger_keywords_case_insensitive_from_config() {
        let t = ResearchTrigger::from_config("keywords", &[
            "Billing".to_string(),
            "INVOICE".to_string(),
        ]);
        assert!(t.should_run("i need billing help"));
        assert!(t.should_run("what is my invoice status"));
    }

    #[test]
    fn trigger_length() {
        let t = ResearchTrigger::Length(100);
        assert!(!t.should_run("short message"));
        assert!(t.should_run("a".repeat(101).as_str()));
    }

    #[test]
    fn trigger_from_config() {
        assert!(matches!(
            ResearchTrigger::from_config("always", &[]),
            ResearchTrigger::Always
        ));
        assert!(matches!(
            ResearchTrigger::from_config("question", &[]),
            ResearchTrigger::Question
        ));
        assert!(matches!(
            ResearchTrigger::from_config("never", &[]),
            ResearchTrigger::Never
        ));
    }

    #[test]
    fn trigger_from_config_length_parsing() {
        assert!(matches!(
            ResearchTrigger::from_config("length", &[]),
            ResearchTrigger::Length(200)
        ));
        assert!(matches!(
            ResearchTrigger::from_config("length:512", &[]),
            ResearchTrigger::Length(512)
        ));
        assert!(matches!(
            ResearchTrigger::from_config(" LENGTH : 42 ", &[]),
            ResearchTrigger::Length(42)
        ));
    }
}
