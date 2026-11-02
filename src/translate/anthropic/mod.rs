//! Tipos de fio do Anthropic Messages e tradução ⇄ OpenAI Chat (PLANO_MESTRE §3.5, ADR 0006).
//!
//! Só o que o gateway **lê ou escreve** é tipado. O que a tradução descarta
//! (`cache_control`, `metadata`, `top_k`, `thinking`, `betas`, …) nem entra nos
//! tipos: o serde ignora campo desconhecido, e bloco de tipo desconhecido vira
//! [`ContentBlock::Unknown`]. O passthrough de `claude-*` não passa por aqui:
//! ele repassa os bytes do cliente.
//!
//! CONTRACT (F1-1): [`OpenAiToAnthropicStream`], [`encode_tool_use_id`] e
//! [`collect_response`] ainda são stubs (F1-3); os testes de aceite deles estão
//! com `#[ignore = "F1-3"]`.

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use super::openai::{ChatCompletionChunk, Usage};

mod request;
pub use request::to_openai_request;

/// Prefixo do texto de um `tool_result` com `is_error: true`: a mensagem `tool`
/// do OpenAI não tem campo para isso.
pub const TOOL_ERROR_PREFIX: &str = "[tool error] ";

/// Separa o `id` do upstream da assinatura de raciocínio no `tool_use.id` que o
/// cliente recebe e devolve (ADR 0006). Só usa caracteres aceitos em `tool_use.id`.
pub const TOOL_ID_SIGNATURE_SEP: &str = "__kaxsig_";

// ------------------------------------------------------------------ request

/// Corpo de `POST /v1/messages` e `/v1/messages/count_tokens` (neste, `max_tokens`
/// não vem).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub stop_sequences: Vec<String>,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

impl MessagesRequest {
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// `system`: string ou lista de blocos de texto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

/// `content` de mensagem: string ou lista de blocos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Bloco de conteúdo, na ida e na volta (na volta só `Text` e `ToolUse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<ToolResultContent>,
        #[serde(
            default,
            deserialize_with = "null_as_default",
            skip_serializing_if = "is_false"
        )]
        is_error: bool,
    },
    /// Descartado na ida; nunca gerado na volta (§3.5).
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    /// Idem.
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    /// `document`, `server_tool_use`, …: nenhum provider da rota entende.
    #[serde(other)]
    Unknown,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `null` explícito vale o mesmo que o campo ausente.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
    #[serde(other)]
    Unknown,
}

/// `content` de `tool_result`: string ou lista de blocos (`text`, `image`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Definição de tool. Tool com `type` próprio (`web_search_…`, `bash_…`,
/// `mcp_toolset`) não tem `input_schema` (e pode não ter `name`): o provider não
/// saberia o que fazer com ela, então não vira function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(
            default,
            deserialize_with = "null_as_default",
            skip_serializing_if = "is_false"
        )]
        disable_parallel_tool_use: bool,
    },
    Any {
        #[serde(
            default,
            deserialize_with = "null_as_default",
            skip_serializing_if = "is_false"
        )]
        disable_parallel_tool_use: bool,
    },
    Tool {
        name: String,
        #[serde(
            default,
            deserialize_with = "null_as_default",
            skip_serializing_if = "is_false"
        )]
        disable_parallel_tool_use: bool,
    },
    None,
}

// ----------------------------------------------------------------- resposta

/// Resposta não-stream; também é o `message` de `message_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: MessageKind,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: MessageUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    Refusal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ------------------------------------------------------------- eventos SSE

/// Evento do stream de `/v1/messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessagesResponse,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: DeltaUsage,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
}

impl StreamEvent {
    /// Valor do campo `event:` do SSE (igual ao `type` do JSON).
    pub fn name(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
}

/// `usage` de `message_delta`. `input_tokens` só sai quando o upstream informou:
/// corrige a estimativa enviada em `message_start`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    pub output_tokens: u32,
}

/// `error` de um evento SSE e do corpo de erro HTTP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// Corpo de erro HTTP: `{"type":"error","error":{"type","message"}}`.
pub fn error_body(kind: &str, message: &str) -> Value {
    let mut error = Map::new();
    error.insert("type".into(), kind.into());
    error.insert("message".into(), message.into());
    let mut body = Map::new();
    body.insert("type".into(), "error".into());
    body.insert("error".into(), Value::Object(error));
    Value::Object(body)
}

/// Um evento no enquadramento SSE: `event: <nome>\ndata: <json>\n\n`.
pub fn encode_sse(event: &StreamEvent) -> Bytes {
    let json = serde_json::to_string(event).unwrap_or_default();
    Bytes::from(format!("event: {}\ndata: {json}\n\n", event.name()))
}

// ---------------------------------------------------------------- tradução

/// Junta o `id` do upstream e a assinatura de raciocínio no `tool_use.id` que vai
/// ao cliente. Sem assinatura, o `id` passa igual. Stub do CONTRACT (F1-3).
pub fn encode_tool_use_id(id: &str, _signature: Option<&str>) -> String {
    id.to_owned()
}

/// Inverso de [`encode_tool_use_id`]: `id` original e assinatura, se havia.
/// Sufixo que não é hexadecimal de um texto UTF-8 não é nosso: o `id` fica como veio.
pub fn decode_tool_use_id(id: &str) -> (String, Option<String>) {
    let decoded = id
        .rsplit_once(TOOL_ID_SIGNATURE_SEP)
        .filter(|(clean, _)| !clean.is_empty())
        .and_then(|(clean, hex)| Some((clean, decode_hex(hex)?)));
    match decoded {
        Some((clean, signature)) => (clean.to_owned(), Some(signature)),
        None => (id.to_owned(), None),
    }
}

fn decode_hex(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[hi, lo]| {
            let hi = char::from(hi).to_digit(16)?;
            let lo = char::from(lo).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// Identificação da resposta que o tradutor de stream vai montar.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamMeta {
    /// `id` da mensagem (`msg_…`).
    pub message_id: String,
    /// Modelo que o cliente pediu (classe ou alias), não o do upstream.
    pub model: String,
    /// `usage.input_tokens` de `message_start`.
    pub est_input_tokens: u32,
    /// Estima `output_tokens` se o upstream não informar `usage`.
    pub chars_per_token: f64,
}

/// Chunks OpenAI → eventos Anthropic, sem I/O.
///
/// Espera chunks já normalizados pelo despacho: raciocínio removido e índices de
/// `tool_calls` estáveis. Os blocos saem em sequência, um aberto por vez: o que
/// chega intercalado fica retido até o bloco aberto fechar (ADR 0006).
///
/// Stub do CONTRACT (F1-3).
#[derive(Debug)]
pub struct OpenAiToAnthropicStream {
    _meta: StreamMeta,
}

impl OpenAiToAnthropicStream {
    pub fn new(meta: StreamMeta) -> Self {
        Self { _meta: meta }
    }

    /// Um chunk do upstream. O 1º chamado emite `message_start`.
    pub fn push(&mut self, _chunk: &ChatCompletionChunk) -> Vec<StreamEvent> {
        Vec::new()
    }

    /// Fim normal do upstream: fecha os blocos, `message_delta`, `message_stop`.
    /// `usage` é o último informado pelo upstream, se houve.
    pub fn finish(&mut self, _usage: Option<Usage>) -> Vec<StreamEvent> {
        Vec::new()
    }

    /// Upstream morreu depois do 1º token: evento `error` e nada mais depois dele.
    pub fn fail(&mut self, _kind: &str, _message: &str) -> Vec<StreamEvent> {
        Vec::new()
    }
}

/// Dobra os eventos de uma resposta completa na mensagem não-stream. Evento
/// `error` vira `Err`. Stub do CONTRACT (F1-3).
pub fn collect_response(_events: &[StreamEvent]) -> Result<MessagesResponse, ApiError> {
    Err(ApiError {
        kind: "api_error".into(),
        message: "tradução ainda não implementada (F1-3)".into(),
    })
}

#[cfg(test)]
mod tests;
