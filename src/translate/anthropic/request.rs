//! Request Anthropic → OpenAI (F1-2, ADR 0006). Função pura: nem scheduler nem HTTP.

use serde_json::{Map, Value, json};

use super::{
    ContentBlock, ImageSource, Message, MessageContent, MessagesRequest, Role, SystemPrompt,
    TOOL_ERROR_PREFIX, Tool, ToolChoice, ToolResultContent, decode_tool_use_id,
};
use crate::translate::openai::{self, ChatCompletionRequest, ChatMessage};

/// Anthropic → OpenAI. `model` continua sendo a classe; quem troca pelo modelo
/// do degrau é o despacho.
pub fn to_openai_request(req: &MessagesRequest) -> ChatCompletionRequest {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = system_text(req.system.as_ref()) {
        messages.push(plain("system", openai::MessageContent::Text(system)));
    }
    for message in &req.messages {
        match message.role {
            Role::User => push_user(message, &mut messages),
            Role::Assistant => push_assistant(message, &mut messages),
        }
    }

    let mut extra = Map::new();
    if let Some(t) = req.temperature {
        extra.insert("temperature".into(), t.into());
    }
    if let Some(p) = req.top_p {
        extra.insert("top_p".into(), p.into());
    }
    if !req.stop_sequences.is_empty() {
        extra.insert("stop".into(), json!(req.stop_sequences));
    }
    let tools = functions(&req.tools);
    if !tools.is_empty() {
        insert_tool_choice(req.tool_choice.as_ref(), &tools, &mut extra);
        extra.insert("tools".into(), Value::Array(tools));
    }

    ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        max_tokens: req.max_tokens,
        extra,
    }
}

fn plain(role: &str, content: openai::MessageContent) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content: Some(content),
        extra: Map::new(),
    }
}

/// Une textos não vazios; nada sobrou → `None`.
fn join_texts<'a>(texts: impl Iterator<Item = &'a str>, sep: &str) -> Option<String> {
    let kept: Vec<&str> = texts.filter(|t| !t.is_empty()).collect();
    (!kept.is_empty()).then(|| kept.join(sep))
}

fn system_text(system: Option<&SystemPrompt>) -> Option<String> {
    match system? {
        SystemPrompt::Text(t) => join_texts(std::iter::once(t.as_str()), ""),
        SystemPrompt::Blocks(blocks) => join_texts(block_texts(blocks), "\n\n"),
    }
}

fn block_texts(blocks: &[ContentBlock]) -> impl Iterator<Item = &str> {
    blocks.iter().filter_map(|b| match b {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

/// Parte `image_url`, ou `None` para fonte desconhecida (ADR 0006: some, não derruba).
fn image_part(source: &ImageSource) -> Option<Value> {
    let url = match source {
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        ImageSource::Url { url } => url.clone(),
        ImageSource::Unknown => return None,
    };
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

fn push_user(message: &Message, out: &mut Vec<ChatMessage>) {
    let blocks = match &message.content {
        MessageContent::Text(t) if t.is_empty() => return,
        MessageContent::Text(t) => {
            out.push(plain("user", openai::MessageContent::Text(t.clone())));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };

    // `tool` tem de vir logo depois de `tool_calls`; o que é do usuário vai depois.
    // Imagem de `tool_result` entra antes do que o usuário escreveu, porque foi a
    // tool que a produziu.
    let mut images = Vec::new();
    for block in blocks {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        {
            let text = tool_result_text(content.as_ref(), *is_error, &mut images);
            let mut tool = plain("tool", openai::MessageContent::Text(text));
            let (id, _) = decode_tool_use_id(tool_use_id);
            tool.extra.insert("tool_call_id".into(), id.into());
            out.push(tool);
        }
    }

    let mut parts = images;
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                parts.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Image { source } => parts.extend(image_part(source)),
            _ => {}
        }
    }
    if parts.iter().any(|p| p["type"] == "image_url") {
        out.push(plain("user", openai::MessageContent::Parts(parts)));
    } else if let Some(text) = join_texts(block_texts(blocks), "\n\n") {
        out.push(plain("user", openai::MessageContent::Text(text)));
    }
}

/// Texto da mensagem `tool`; as imagens do resultado vão para `images`.
fn tool_result_text(
    content: Option<&ToolResultContent>,
    is_error: bool,
    images: &mut Vec<Value>,
) -> String {
    let text = match content {
        None => String::new(),
        Some(ToolResultContent::Text(t)) => t.clone(),
        Some(ToolResultContent::Blocks(blocks)) => {
            images.extend(blocks.iter().filter_map(|b| match b {
                ContentBlock::Image { source } => image_part(source),
                _ => None,
            }));
            join_texts(block_texts(blocks), "\n").unwrap_or_default()
        }
    };
    if is_error {
        format!("{TOOL_ERROR_PREFIX}{text}")
    } else {
        text
    }
}

fn push_assistant(message: &Message, out: &mut Vec<ChatMessage>) {
    let blocks = match &message.content {
        MessageContent::Text(t) if t.is_empty() => return,
        MessageContent::Text(t) => {
            out.push(plain("assistant", openai::MessageContent::Text(t.clone())));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };

    let calls: Vec<Value> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(tool_call(id, name, input)),
            _ => None,
        })
        .collect();
    let text = join_texts(block_texts(blocks), "\n\n");
    if text.is_none() && calls.is_empty() {
        return;
    }

    let content = text.map_or(openai::MessageContent::Null, openai::MessageContent::Text);
    let mut assistant = plain("assistant", content);
    if !calls.is_empty() {
        assistant
            .extra
            .insert("tool_calls".into(), Value::Array(calls));
    }
    out.push(assistant);
}

fn tool_call(id: &str, name: &str, input: &Value) -> Value {
    let (id, signature) = decode_tool_use_id(id);
    let arguments = if input.is_null() {
        "{}".to_owned()
    } else {
        input.to_string()
    };
    let mut call = json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    });
    if let Some(signature) = signature {
        call["extra_content"] = json!({ "google": { "thought_signature": signature } });
    }
    call
}

/// Só tool com nome e `input_schema` vira function (ADR 0006).
fn functions(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .filter(|t| !t.name.is_empty())
        .filter_map(|t| {
            let schema = t.input_schema.as_ref()?;
            let mut function = Map::new();
            function.insert("name".into(), t.name.clone().into());
            if let Some(description) = &t.description {
                function.insert("description".into(), description.clone().into());
            }
            function.insert("parameters".into(), schema.clone());
            Some(json!({ "type": "function", "function": function }))
        })
        .collect()
}

fn insert_tool_choice(
    choice: Option<&ToolChoice>,
    tools: &[Value],
    extra: &mut Map<String, Value>,
) {
    let (value, disable_parallel) = match choice {
        None => return,
        Some(ToolChoice::Auto {
            disable_parallel_tool_use,
        }) => (json!("auto"), *disable_parallel_tool_use),
        Some(ToolChoice::Any {
            disable_parallel_tool_use,
        }) => (json!("required"), *disable_parallel_tool_use),
        Some(ToolChoice::None) => (json!("none"), false),
        Some(ToolChoice::Tool {
            name,
            disable_parallel_tool_use,
        }) => {
            // Tool de servidor não vira function: forçá-la daria 400 no upstream.
            if !tools.iter().any(|t| t["function"]["name"] == *name) {
                return;
            }
            (
                json!({ "type": "function", "function": { "name": name } }),
                *disable_parallel_tool_use,
            )
        }
    };
    extra.insert("tool_choice".into(), value);
    if disable_parallel {
        extra.insert("parallel_tool_calls".into(), false.into());
    }
}
