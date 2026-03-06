//! Typed message structures for session storage.
//!
//! These types represent the JSON format stored in session JSONL files.
//! They include both LLM-relevant fields (role, content) and metadata
//! fields (created_at, model, provider, tokens, channel).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A message stored in a session JSONL file.
///
/// Includes both the LLM-relevant content and metadata for UI display
/// and analytics. The `role` field determines which variant this is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum PersistedMessage {
    System {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
    },
    /// UI-only informational message (not part of LLM prompt history).
    Notice {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
    },
    User {
        /// Content can be a string (plain text) or array (multimodal).
        content: MessageContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
        /// Relative media path for uploaded user audio (e.g. "media/main/voice-123.webm").
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        /// Channel metadata for UI display (e.g., Telegram sender info).
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<Value>,
        /// Client-assigned sequence number for ordering diagnostics.
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Agent run ID that processes this message (parent→child link).
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Total input tokens spent during this assistant turn.
        #[serde(rename = "inputTokens", skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u32>,
        /// Total output tokens produced during this assistant turn.
        #[serde(rename = "outputTokens", skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u32>,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Input tokens sent in the final LLM request for this turn.
        #[serde(rename = "requestInputTokens", skip_serializing_if = "Option::is_none")]
        request_input_tokens: Option<u32>,
        /// Output tokens produced in the final LLM request for this turn.
        #[serde(
            rename = "requestOutputTokens",
            skip_serializing_if = "Option::is_none"
        )]
        request_output_tokens: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<PersistedToolCall>>,
        /// Optional provider reasoning/planning text (not final answer text).
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        /// Raw provider API payload captured during streaming for debugging.
        #[serde(rename = "llmApiResponse", skip_serializing_if = "Option::is_none")]
        llm_api_response: Option<Value>,
        /// Relative media path for TTS audio (e.g. "media/main/run_abc.ogg").
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
        /// Sequence number matching the user message this responds to.
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Agent run ID linking this response to its parent user message.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
    },
    /// Tool execution result with structured output (stdout, stderr, exit_code).
    ///
    /// Persisted alongside user/assistant messages so that the UI can
    /// reconstruct exec cards when a session is reloaded.
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Provider reasoning/thinking text that preceded this tool call.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
        /// Agent run ID linking this result to its parent run.
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
}

/// User message content: plain text or multimodal array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Multimodal(Vec<ContentBlock>),
}

/// A single block in multimodal content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

/// Image URL data (for multimodal content).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

/// A tool call stored in an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: PersistedFunction,
}

/// Function details in a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedFunction {
    pub name: String,
    pub arguments: String,
}

impl PersistedMessage {
    /// Create a user message with plain text content.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: MessageContent::Text(text.into()),
            created_at: Some(now_ms()),
            audio: None,
            channel: None,
            seq: None,
            run_id: None,
        }
    }

    /// Create a user message with plain text and channel metadata.
    pub fn user_with_channel(text: impl Into<String>, channel: Value) -> Self {
        Self::User {
            content: MessageContent::Text(text.into()),
            created_at: Some(now_ms()),
            audio: None,
            channel: Some(channel),
            seq: None,
            run_id: None,
        }
    }

    /// Create a user message with multimodal content.
    pub fn user_multimodal(blocks: Vec<ContentBlock>) -> Self {
        Self::User {
            content: MessageContent::Multimodal(blocks),
            created_at: Some(now_ms()),
            audio: None,
            channel: None,
            seq: None,
            run_id: None,
        }
    }

    /// Create a user message with multimodal content and channel metadata.
    pub fn user_multimodal_with_channel(blocks: Vec<ContentBlock>, channel: Value) -> Self {
        Self::User {
            content: MessageContent::Multimodal(blocks),
            created_at: Some(now_ms()),
            audio: None,
            channel: Some(channel),
            seq: None,
            run_id: None,
        }
    }

    /// Create an assistant message with token usage and model info.
    pub fn assistant(
        text: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        input_tokens: u32,
        output_tokens: u32,
        audio: Option<String>,
    ) -> Self {
        Self::Assistant {
            content: text.into(),
            created_at: Some(now_ms()),
            model: Some(model.into()),
            provider: Some(provider.into()),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            duration_ms: None,
            request_input_tokens: Some(input_tokens),
            request_output_tokens: Some(output_tokens),
            tool_calls: None,
            reasoning: None,
            llm_api_response: None,
            audio,
            seq: None,
            run_id: None,
        }
    }

    /// Create a system message (e.g., for error display).
    pub fn system(text: impl Into<String>) -> Self {
        Self::System {
            content: text.into(),
            created_at: Some(now_ms()),
        }
    }

    /// Create a notice message shown in UI but skipped from model context.
    pub fn notice(text: impl Into<String>) -> Self {
        Self::Notice {
            content: text.into(),
            created_at: Some(now_ms()),
        }
    }

    /// Create a tool result message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            created_at: Some(now_ms()),
        }
    }

    /// Create a tool execution result message.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Option<Value>,
        success: bool,
        result: Option<Value>,
        error: Option<String>,
    ) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            success,
            result,
            error,
            reasoning: None,
            created_at: Some(now_ms()),
            run_id: None,
        }
    }

    /// Create a tool execution result message with reasoning text.
    pub fn tool_result_with_reasoning(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Option<Value>,
        success: bool,
        result: Option<Value>,
        error: Option<String>,
        reasoning: Option<String>,
    ) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            success,
            result,
            error,
            reasoning,
            created_at: Some(now_ms()),
            run_id: None,
        }
    }

    /// Create a tool result message with a run ID linking it to its agent run.
    pub fn tool_result_with_run_id(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Option<Value>,
        success: bool,
        result: Option<Value>,
        error: Option<String>,
        run_id: impl Into<String>,
    ) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            success,
            result,
            error,
            reasoning: None,
            created_at: Some(now_ms()),
            run_id: Some(run_id.into()),
        }
    }

    /// Convert to JSON value for storage.
    ///
    /// This cannot fail because `PersistedMessage` only contains types with
    /// infallible serialization (strings, numbers, booleans, vecs, options).
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();

        match self {
            Self::System {
                content,
                created_at,
            } => {
                object.insert("role".to_string(), Value::String("system".to_string()));
                object.insert("content".to_string(), Value::String(content.clone()));
                insert_optional_u64(&mut object, "created_at", *created_at);
            },
            Self::Notice {
                content,
                created_at,
            } => {
                object.insert("role".to_string(), Value::String("notice".to_string()));
                object.insert("content".to_string(), Value::String(content.clone()));
                insert_optional_u64(&mut object, "created_at", *created_at);
            },
            Self::User {
                content,
                created_at,
                audio,
                channel,
                seq,
                run_id,
            } => {
                object.insert("role".to_string(), Value::String("user".to_string()));
                object.insert("content".to_string(), message_content_to_value(content));
                insert_optional_u64(&mut object, "created_at", *created_at);
                insert_optional_string(&mut object, "audio", audio.as_ref());
                insert_optional_value(&mut object, "channel", channel.as_ref());
                insert_optional_u64(&mut object, "seq", *seq);
                insert_optional_string(&mut object, "run_id", run_id.as_ref());
            },
            Self::Assistant {
                content,
                created_at,
                model,
                provider,
                input_tokens,
                output_tokens,
                duration_ms,
                request_input_tokens,
                request_output_tokens,
                tool_calls,
                reasoning,
                llm_api_response,
                audio,
                seq,
                run_id,
            } => {
                object.insert("role".to_string(), Value::String("assistant".to_string()));
                object.insert("content".to_string(), Value::String(content.clone()));
                insert_optional_u64(&mut object, "created_at", *created_at);
                insert_optional_string(&mut object, "model", model.as_ref());
                insert_optional_string(&mut object, "provider", provider.as_ref());
                insert_optional_u32(&mut object, "inputTokens", *input_tokens);
                insert_optional_u32(&mut object, "outputTokens", *output_tokens);
                insert_optional_u64(&mut object, "durationMs", *duration_ms);
                insert_optional_u32(&mut object, "requestInputTokens", *request_input_tokens);
                insert_optional_u32(&mut object, "requestOutputTokens", *request_output_tokens);
                insert_optional_tool_calls(&mut object, tool_calls.as_deref());
                insert_optional_string(&mut object, "reasoning", reasoning.as_ref());
                insert_optional_value(&mut object, "llmApiResponse", llm_api_response.as_ref());
                insert_optional_string(&mut object, "audio", audio.as_ref());
                insert_optional_u64(&mut object, "seq", *seq);
                insert_optional_string(&mut object, "run_id", run_id.as_ref());
            },
            Self::Tool {
                tool_call_id,
                content,
                created_at,
            } => {
                object.insert("role".to_string(), Value::String("tool".to_string()));
                object.insert(
                    "tool_call_id".to_string(),
                    Value::String(tool_call_id.clone()),
                );
                object.insert("content".to_string(), Value::String(content.clone()));
                insert_optional_u64(&mut object, "created_at", *created_at);
            },
            Self::ToolResult {
                tool_call_id,
                tool_name,
                arguments,
                success,
                result,
                error,
                reasoning,
                created_at,
                run_id,
            } => {
                object.insert("role".to_string(), Value::String("tool_result".to_string()));
                object.insert(
                    "tool_call_id".to_string(),
                    Value::String(tool_call_id.clone()),
                );
                object.insert("tool_name".to_string(), Value::String(tool_name.clone()));
                insert_optional_value(&mut object, "arguments", arguments.as_ref());
                object.insert("success".to_string(), Value::Bool(*success));
                insert_optional_value(&mut object, "result", result.as_ref());
                insert_optional_string(&mut object, "error", error.as_ref());
                insert_optional_string(&mut object, "reasoning", reasoning.as_ref());
                insert_optional_u64(&mut object, "created_at", *created_at);
                insert_optional_string(&mut object, "run_id", run_id.as_ref());
            },
        }

        Value::Object(object)
    }
}

impl ContentBlock {
    /// Create a text content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Create an image URL content block from base64 data.
    pub fn image_base64(media_type: &str, data: &str) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{media_type};base64,{data}"),
            },
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn message_content_to_value(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => Value::String(text.clone()),
        MessageContent::Multimodal(blocks) => {
            Value::Array(blocks.iter().map(content_block_to_value).collect())
        },
    }
}

fn content_block_to_value(block: &ContentBlock) -> Value {
    let mut object = Map::new();
    match block {
        ContentBlock::Text { text } => {
            object.insert("type".to_string(), Value::String("text".to_string()));
            object.insert("text".to_string(), Value::String(text.clone()));
        },
        ContentBlock::ImageUrl { image_url } => {
            object.insert("type".to_string(), Value::String("image_url".to_string()));
            object.insert(
                "image_url".to_string(),
                Value::Object(Map::from_iter([(
                    "url".to_string(),
                    Value::String(image_url.url.clone()),
                )])),
            );
        },
    }
    Value::Object(object)
}

fn tool_call_to_value(tool_call: &PersistedToolCall) -> Value {
    Value::Object(Map::from_iter([
        ("id".to_string(), Value::String(tool_call.id.clone())),
        (
            "type".to_string(),
            Value::String(tool_call.call_type.clone()),
        ),
        (
            "function".to_string(),
            Value::Object(Map::from_iter([
                (
                    "name".to_string(),
                    Value::String(tool_call.function.name.clone()),
                ),
                (
                    "arguments".to_string(),
                    Value::String(tool_call.function.arguments.clone()),
                ),
            ])),
        ),
    ]))
}

fn insert_optional_string(object: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::String(value.clone()));
    }
}

fn insert_optional_u32(object: &mut Map<String, Value>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::from(value));
    }
}

fn insert_optional_u64(object: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::from(value));
    }
}

fn insert_optional_value(object: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        object.insert(key.to_string(), value.clone());
    }
}

fn insert_optional_tool_calls(
    object: &mut Map<String, Value>,
    tool_calls: Option<&[PersistedToolCall]>,
) {
    if let Some(tool_calls) = tool_calls {
        object.insert(
            "tool_calls".to_string(),
            Value::Array(tool_calls.iter().map(tool_call_to_value).collect()),
        );
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_serializes_correctly() {
        let msg = PersistedMessage::User {
            content: MessageContent::Text("hello".to_string()),
            created_at: Some(12345),
            audio: None,
            channel: None,
            seq: None,
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
        assert_eq!(json["created_at"], 12345);
        assert!(json.get("channel").is_none());
    }

    #[test]
    fn user_multimodal_serializes_correctly() {
        let msg = PersistedMessage::User {
            content: MessageContent::Multimodal(vec![
                ContentBlock::text("describe this"),
                ContentBlock::image_base64("image/jpeg", "abc123"),
            ]),
            created_at: Some(12345),
            audio: None,
            channel: None,
            seq: None,
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        let content = json["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe this");
        assert_eq!(content[1]["type"], "image_url");
        assert!(
            content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
    }

    #[test]
    fn assistant_serializes_correctly() {
        let msg = PersistedMessage::Assistant {
            content: "response".to_string(),
            created_at: Some(12345),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            input_tokens: Some(100),
            output_tokens: Some(50),
            duration_ms: Some(2_000),
            request_input_tokens: Some(100),
            request_output_tokens: Some(50),
            tool_calls: None,
            reasoning: None,
            llm_api_response: None,
            audio: None,
            seq: None,
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "response");
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["inputTokens"], 100);
        assert_eq!(json["outputTokens"], 50);
        assert_eq!(json["durationMs"], 2_000);
        assert_eq!(json["requestInputTokens"], 100);
        assert_eq!(json["requestOutputTokens"], 50);
        assert!(json.get("audio").is_none());
    }

    #[test]
    fn notice_serializes_correctly() {
        let msg = PersistedMessage::Notice {
            content: "shared cutoff".to_string(),
            created_at: Some(12345),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "notice");
        assert_eq!(json["content"], "shared cutoff");
        assert_eq!(json["created_at"], 12345);
    }

    #[test]
    fn user_text_deserializes_correctly() {
        let json = serde_json::json!({
            "role": "user",
            "content": "hello",
            "created_at": 12345
        });
        let msg: PersistedMessage = serde_json::from_value(json).unwrap();
        match msg {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, MessageContent::Text(t) if t == "hello"));
            },
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn user_with_audio_serializes_correctly() {
        let msg = PersistedMessage::User {
            content: MessageContent::Text("voice note".to_string()),
            created_at: Some(12345),
            audio: Some("media/main/voice-123.webm".to_string()),
            channel: None,
            seq: None,
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "voice note");
        assert_eq!(json["audio"], "media/main/voice-123.webm");
    }

    #[test]
    fn user_without_audio_field_deserializes() {
        let json = serde_json::json!({
            "role": "user",
            "content": "old user message",
            "created_at": 12345
        });
        let msg: PersistedMessage = serde_json::from_value(json).unwrap();
        match msg {
            PersistedMessage::User { content, audio, .. } => {
                assert!(matches!(content, MessageContent::Text(t) if t == "old user message"));
                assert!(audio.is_none());
            },
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn user_multimodal_deserializes_correctly() {
        let json = serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "describe" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,xyz" } }
            ]
        });
        let msg: PersistedMessage = serde_json::from_value(json).unwrap();
        match msg {
            PersistedMessage::User { content, .. } => match content {
                MessageContent::Multimodal(blocks) => {
                    assert_eq!(blocks.len(), 2);
                },
                _ => panic!("expected multimodal content"),
            },
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn roundtrip_user_text() {
        let original = PersistedMessage::user("test message");
        let json = original.to_value();
        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::User { content, .. } => {
                assert!(matches!(content, MessageContent::Text(t) if t == "test message"));
            },
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn roundtrip_notice() {
        let original = PersistedMessage::notice("snapshot cutoff");
        let json = original.to_value();
        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::Notice { content, .. } => {
                assert_eq!(content, "snapshot cutoff");
            },
            _ => panic!("expected Notice message"),
        }
    }

    #[test]
    fn roundtrip_assistant() {
        let original = PersistedMessage::assistant("response", "gpt-4o", "openai", 100, 50, None);
        let json = original.to_value();
        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::Assistant {
                content,
                model,
                provider,
                input_tokens,
                output_tokens,
                request_input_tokens,
                request_output_tokens,
                reasoning,
                audio,
                ..
            } => {
                assert_eq!(content, "response");
                assert_eq!(model.as_deref(), Some("gpt-4o"));
                assert_eq!(provider.as_deref(), Some("openai"));
                assert_eq!(input_tokens, Some(100));
                assert_eq!(output_tokens, Some(50));
                assert_eq!(request_input_tokens, Some(100));
                assert_eq!(request_output_tokens, Some(50));
                assert!(reasoning.is_none());
                assert!(audio.is_none());
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn roundtrip_assistant_with_audio() {
        let original = PersistedMessage::assistant(
            "hello world",
            "gpt-4o",
            "openai",
            80,
            20,
            Some("media/main/run_abc.ogg".to_string()),
        );
        let json = original.to_value();
        assert_eq!(json["audio"], "media/main/run_abc.ogg");
        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::Assistant { audio, .. } => {
                assert_eq!(audio.as_deref(), Some("media/main/run_abc.ogg"));
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn assistant_without_audio_field_deserializes() {
        // Old sessions without audio field should still parse correctly.
        let json = serde_json::json!({
            "role": "assistant",
            "content": "old message",
            "model": "gpt-4o",
            "provider": "openai",
            "inputTokens": 50,
            "outputTokens": 25,
            "created_at": 12345
        });
        let msg: PersistedMessage = serde_json::from_value(json).unwrap();
        match msg {
            PersistedMessage::Assistant {
                audio,
                content,
                request_input_tokens,
                request_output_tokens,
                ..
            } => {
                assert_eq!(content, "old message");
                assert!(audio.is_none());
                assert!(request_input_tokens.is_none());
                assert!(request_output_tokens.is_none());
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn tool_result_serializes_correctly() {
        let msg = PersistedMessage::ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: "exec".to_string(),
            arguments: Some(serde_json::json!({"command": "ls -la"})),
            success: true,
            result: Some(serde_json::json!({"stdout": "file.txt", "exit_code": 0})),
            error: None,
            reasoning: None,
            created_at: Some(12345),
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "tool_result");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["tool_name"], "exec");
        assert_eq!(json["arguments"]["command"], "ls -la");
        assert!(json["success"].as_bool().unwrap());
        assert_eq!(json["result"]["stdout"], "file.txt");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn tool_result_error_serializes_correctly() {
        let msg = PersistedMessage::ToolResult {
            tool_call_id: "call_2".to_string(),
            tool_name: "exec".to_string(),
            arguments: Some(serde_json::json!({"command": "bad_cmd"})),
            success: false,
            result: None,
            error: Some("command not found".to_string()),
            reasoning: None,
            created_at: Some(12345),
            run_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "tool_result");
        assert!(!json["success"].as_bool().unwrap());
        assert_eq!(json["error"], "command not found");
        assert!(json.get("result").is_none());
    }

    #[test]
    fn roundtrip_tool_result() {
        let original = PersistedMessage::tool_result(
            "call_3",
            "web_fetch",
            Some(serde_json::json!({"url": "https://example.com"})),
            true,
            Some(serde_json::json!({"stdout": "OK", "exit_code": 0})),
            None,
        );
        let json = original.to_value();
        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::ToolResult {
                tool_call_id,
                tool_name,
                arguments,
                success,
                result,
                error,
                ..
            } => {
                assert_eq!(tool_call_id, "call_3");
                assert_eq!(tool_name, "web_fetch");
                assert_eq!(arguments.unwrap()["url"], "https://example.com");
                assert!(success);
                assert_eq!(result.unwrap()["stdout"], "OK");
                assert!(error.is_none());
            },
            _ => panic!("expected ToolResult message"),
        }
    }

    #[test]
    fn tool_result_deserializes_from_json() {
        let json = serde_json::json!({
            "role": "tool_result",
            "tool_call_id": "call_4",
            "tool_name": "exec",
            "success": true,
            "result": {"stdout": "hello", "stderr": "", "exit_code": 0},
            "created_at": 99999
        });
        let msg: PersistedMessage = serde_json::from_value(json).unwrap();
        match msg {
            PersistedMessage::ToolResult {
                tool_call_id,
                tool_name,
                success,
                reasoning,
                ..
            } => {
                assert_eq!(tool_call_id, "call_4");
                assert_eq!(tool_name, "exec");
                assert!(success);
                // Old sessions without reasoning field should deserialize as None.
                assert!(reasoning.is_none());
            },
            _ => panic!("expected ToolResult message"),
        }
    }

    #[test]
    fn tool_result_with_reasoning_roundtrips() {
        let original = PersistedMessage::tool_result_with_reasoning(
            "call_5",
            "web_search",
            Some(serde_json::json!({"query": "top news"})),
            true,
            Some(serde_json::json!({"stdout": "results", "exit_code": 0})),
            None,
            Some("I need to search for today's news".to_string()),
        );
        let json = original.to_value();
        assert_eq!(json["reasoning"], "I need to search for today's news");

        let parsed: PersistedMessage = serde_json::from_value(json).unwrap();
        match parsed {
            PersistedMessage::ToolResult {
                tool_call_id,
                reasoning,
                ..
            } => {
                assert_eq!(tool_call_id, "call_5");
                assert_eq!(
                    reasoning.as_deref(),
                    Some("I need to search for today's news")
                );
            },
            _ => panic!("expected ToolResult message"),
        }
    }

    #[test]
    fn tool_result_without_reasoning_omits_field() {
        let msg = PersistedMessage::tool_result(
            "call_6",
            "exec",
            None,
            true,
            Some(serde_json::json!({"stdout": "ok"})),
            None,
        );
        let json = msg.to_value();
        // reasoning field should not be present when None.
        assert!(json.get("reasoning").is_none());
    }

    #[test]
    fn manual_to_value_matches_serde_shape() {
        let cases = vec![
            PersistedMessage::system("system prompt"),
            PersistedMessage::notice("ui-only notice"),
            PersistedMessage::User {
                content: MessageContent::Multimodal(vec![
                    ContentBlock::text("describe this"),
                    ContentBlock::image_base64("image/png", "abc123"),
                ]),
                created_at: Some(42),
                audio: Some("media/main/voice.webm".to_string()),
                channel: Some(serde_json::json!({"sender": "alice"})),
                seq: Some(7),
                run_id: Some("run-1".to_string()),
            },
            PersistedMessage::Assistant {
                content: "done".to_string(),
                created_at: Some(99),
                model: Some("gpt-5".to_string()),
                provider: Some("openai".to_string()),
                input_tokens: Some(10),
                output_tokens: Some(5),
                duration_ms: Some(123),
                request_input_tokens: Some(9),
                request_output_tokens: Some(4),
                tool_calls: Some(vec![PersistedToolCall {
                    id: "call-1".to_string(),
                    call_type: "function".to_string(),
                    function: PersistedFunction {
                        name: "calc".to_string(),
                        arguments: "{\"x\":1}".to_string(),
                    },
                }]),
                reasoning: Some("thinking".to_string()),
                llm_api_response: Some(serde_json::json!({"raw": true})),
                audio: Some("media/main/reply.ogg".to_string()),
                seq: Some(8),
                run_id: Some("run-2".to_string()),
            },
            PersistedMessage::tool("call-2", "stdout"),
            PersistedMessage::ToolResult {
                tool_call_id: "call-3".to_string(),
                tool_name: "exec".to_string(),
                arguments: Some(serde_json::json!({"command": "pwd"})),
                success: false,
                result: Some(serde_json::json!({"stderr": "boom"})),
                error: Some("boom".to_string()),
                reasoning: Some("need shell".to_string()),
                created_at: Some(77),
                run_id: Some("run-3".to_string()),
            },
        ];

        for message in cases {
            let manual = message.to_value();
            let derived = serde_json::to_value(&message).unwrap();
            assert_eq!(manual, derived);
        }
    }
}
