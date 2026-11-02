//! Tipos de fio do OpenAI Chat Completions.
//!
//! Só os campos que o gateway **lê** são tipados; o resto viaja em `extra`
//! (`#[serde(flatten)]`) e é repassado intacto ao upstream — exceto o que os
//! `quirks` do provider mandarem remover.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Corpo de `POST /v1/chat/completions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// Do cliente: classe (`kax/coder-fast`). Para o upstream: modelo do degrau.
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// `temperature`, `tools`, `tool_choice`, `stop`, `stream_options`, …
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ChatCompletionRequest {
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// Ausente ≠ `null`: o que o cliente mandou é o que o upstream recebe
    /// (`assistant` com `tool_calls` costuma vir com `content: null`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_content"
    )]
    pub content: Option<MessageContent>,
    /// `tool_calls`, `tool_call_id`, `name`, …
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Campo presente vira `Some`, inclusive `null` (`Option` sozinho viraria `None`
/// e o campo sumiria na serialização).
fn present_content<'de, D>(d: D) -> Result<Option<MessageContent>, D::Error>
where
    D: Deserializer<'de>,
{
    MessageContent::deserialize(d).map(Some)
}

/// `content` pode ser string, lista de partes (`text`, `image_url`, …) ou `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<Value>),
    Null,
}

/// Um evento `data:` de um stream SSE de chat completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// F0-24: `content: null` e `content` ausente são formas diferentes e as duas
    /// chegam ao upstream como vieram; o resto do tipo segue igual.
    #[test]
    fn message_content_roundtrip_keeps_null_absent_text_and_parts() {
        let tc = json!([{ "id": "c1", "type": "function", "function": { "name": "f", "arguments": "{}" } }]);
        for m in [
            json!({ "role": "assistant", "content": null, "tool_calls": tc }),
            json!({ "role": "assistant", "tool_calls": tc }),
            json!({ "role": "user", "content": "oi" }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "oi" }] }),
        ] {
            let parsed: ChatMessage = serde_json::from_value(m.clone()).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), m);
        }
    }
}
