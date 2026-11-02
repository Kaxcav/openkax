//! Chunks OpenAI → eventos Anthropic, sem I/O (ADR 0006, "Stream").
//!
//! O que chega do upstream vira uma fila de segmentos na ordem de aparição. O da
//! frente é o único que pode estar aberto no cliente; o resto fica retido, porque
//! o protocolo só admite um bloco aberto por vez e o upstream intercala à vontade.

use std::collections::VecDeque;

use serde_json::{Map, Value};

use super::{
    ApiError, BlockDelta, ContentBlock, DeltaUsage, MessageDeltaBody, MessageKind, MessageUsage,
    MessagesResponse, Role, StopReason, StreamEvent, encode_tool_use_id,
};
use crate::translate::openai::{ChatCompletionChunk, Usage};

/// Mensagem do evento `error` quando o próprio upstream avisa da falha com
/// `finish_reason: "error"` (a morte do stream traz a dela, via `fail`).
const UPSTREAM_FAILED: &str = "o provider encerrou a resposta com erro";

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

/// Cabeçalho de uma chamada de tool, que chega em pedaços e em qualquer ordem.
#[derive(Debug)]
struct ToolHead {
    upstream_index: u64,
    /// Ordem de aparição entre as tools: o `n` do `id` gerado.
    ordinal: usize,
    id: String,
    name: String,
    signature: Option<String>,
}

#[derive(Debug)]
enum SegmentKind {
    Text,
    Tool(ToolHead),
}

/// Um bloco futuro ou aberto. `pending` é o texto ou os argumentos ainda não enviados.
#[derive(Debug)]
struct Segment {
    kind: SegmentKind,
    block: Option<u32>,
    pending: String,
}

impl Segment {
    fn new(kind: SegmentKind) -> Self {
        Self {
            kind,
            block: None,
            pending: String::new(),
        }
    }

    /// Bloco de tool só abre com `id` e `name`.
    fn ready(&self) -> bool {
        match &self.kind {
            SegmentKind::Text => true,
            SegmentKind::Tool(head) => !head.id.is_empty() && !head.name.is_empty(),
        }
    }

    fn start_block(&self) -> ContentBlock {
        match &self.kind {
            SegmentKind::Text => ContentBlock::Text {
                text: String::new(),
            },
            SegmentKind::Tool(head) => ContentBlock::ToolUse {
                id: encode_tool_use_id(&head.id, head.signature.as_deref()),
                name: head.name.clone(),
                input: Value::Object(Map::new()),
            },
        }
    }

    fn delta(&mut self) -> Option<BlockDelta> {
        if self.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut self.pending);
        Some(match self.kind {
            SegmentKind::Text => BlockDelta::TextDelta { text: pending },
            SegmentKind::Tool(_) => BlockDelta::InputJsonDelta {
                partial_json: pending,
            },
        })
    }
}

/// Chunks OpenAI → eventos Anthropic.
///
/// Espera chunks já normalizados pelo despacho: raciocínio removido e índices de
/// `tool_calls` estáveis. Os blocos saem em sequência, um aberto por vez: o que
/// chega intercalado fica retido até o bloco aberto fechar (ADR 0006).
#[derive(Debug)]
pub struct OpenAiToAnthropicStream {
    meta: StreamMeta,
    started: bool,
    ended: bool,
    segments: VecDeque<Segment>,
    next_block: u32,
    next_tool: usize,
    tool_emitted: bool,
    finish_reason: Option<String>,
    output_chars: usize,
}

impl OpenAiToAnthropicStream {
    pub fn new(meta: StreamMeta) -> Self {
        Self {
            meta,
            started: false,
            ended: false,
            segments: VecDeque::new(),
            next_block: 0,
            next_tool: 0,
            tool_emitted: false,
            finish_reason: None,
            output_chars: 0,
        }
    }

    /// Um chunk do upstream. O 1º chamado emite `message_start`.
    pub fn push(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.start(&mut out);
        for choice in chunk.choices.iter().filter(|c| c.index == 0) {
            if let Some(text) = choice.delta.content.as_deref() {
                self.add_text(text);
            }
            for call in choice.delta.tool_calls.iter().flatten() {
                self.add_tool_delta(call);
            }
            // `""` é o `null` de alguns upstreams.
            if let Some(reason) = choice.finish_reason.as_deref().filter(|r| !r.is_empty()) {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        self.pump(false, &mut out);
        if self.finish_reason.as_deref() == Some("error") {
            out.extend(self.fail("api_error", UPSTREAM_FAILED));
        }
        out
    }

    /// Fim normal do upstream: fecha os blocos, `message_delta`, `message_stop`.
    /// `usage` é o último informado pelo upstream, se houve.
    pub fn finish(&mut self, usage: Option<Usage>) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.start(&mut out);
        self.ended = true;
        self.pump(true, &mut out);
        out.push(StreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(self.stop_reason()),
                stop_sequence: None,
            },
            usage: self.delta_usage(usage),
        });
        out.push(StreamEvent::MessageStop);
        out
    }

    /// Upstream morreu depois do 1º token: evento `error` e nada mais depois dele.
    pub fn fail(&mut self, kind: &str, message: &str) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        self.segments.clear();
        vec![StreamEvent::Error {
            error: ApiError {
                kind: kind.to_owned(),
                message: message.to_owned(),
            },
        }]
    }

    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(StreamEvent::MessageStart {
            message: MessagesResponse {
                id: self.meta.message_id.clone(),
                kind: MessageKind::Message,
                role: Role::Assistant,
                content: Vec::new(),
                model: self.meta.model.clone(),
                stop_reason: None,
                stop_sequence: None,
                usage: MessageUsage {
                    input_tokens: self.meta.est_input_tokens,
                    output_tokens: 0,
                },
            },
        });
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.output_chars += text.chars().count();
        let last_is_text = self
            .segments
            .back()
            .is_some_and(|s| matches!(s.kind, SegmentKind::Text));
        if !last_is_text {
            self.segments.push_back(Segment::new(SegmentKind::Text));
        }
        if let Some(last) = self.segments.back_mut() {
            last.pending.push_str(text);
        }
    }

    fn add_tool_delta(&mut self, delta: &Value) {
        let upstream_index = delta["index"].as_u64().unwrap_or(0);
        let found = self.segments.iter().position(
            |s| matches!(&s.kind, SegmentKind::Tool(h) if h.upstream_index == upstream_index),
        );
        let position = found.unwrap_or_else(|| {
            self.segments
                .push_back(Segment::new(SegmentKind::Tool(ToolHead {
                    upstream_index,
                    ordinal: self.next_tool,
                    id: String::new(),
                    name: String::new(),
                    signature: None,
                })));
            self.next_tool += 1;
            self.segments.len() - 1
        });
        let segment = &mut self.segments[position];
        if let SegmentKind::Tool(head) = &mut segment.kind {
            // Há upstream que repete `id` e `name` em todo chunk: vale o 1º.
            set_if_empty(&mut head.id, delta["id"].as_str());
            set_if_empty(&mut head.name, delta["function"]["name"].as_str());
            let signature = delta["extra_content"]["google"]["thought_signature"].as_str();
            if head.signature.is_none() {
                head.signature = signature.filter(|s| !s.is_empty()).map(str::to_owned);
            }
        }
        if let Some(args) = delta["function"]["arguments"].as_str() {
            segment.pending.push_str(args);
            self.output_chars += args.chars().count();
        }
    }

    /// Solta o que a fila permite. Com `finishing`, fecha tudo: `id` ausente é
    /// gerado e chamada que nunca recebeu nome some, porque o cliente não teria o
    /// que executar.
    fn pump(&mut self, finishing: bool, out: &mut Vec<StreamEvent>) {
        while let Some(front) = self.segments.front_mut() {
            if finishing
                && let SegmentKind::Tool(head) = &mut front.kind
                && head.id.is_empty()
            {
                head.id = format!("toolu_{}_{}", self.meta.message_id, head.ordinal);
            }
            let index = match front.block {
                Some(index) => index,
                None if front.ready() => {
                    let index = self.next_block;
                    self.next_block += 1;
                    self.tool_emitted |= matches!(front.kind, SegmentKind::Tool(_));
                    front.block = Some(index);
                    out.push(StreamEvent::ContentBlockStart {
                        index,
                        content_block: front.start_block(),
                    });
                    index
                }
                None if finishing => {
                    self.segments.pop_front();
                    continue;
                }
                None => return,
            };
            if let Some(delta) = front.delta() {
                out.push(StreamEvent::ContentBlockDelta { index, delta });
            }
            // Tool aberta só fecha no fim; texto fecha assim que há algo depois dele.
            let closes =
                finishing || (matches!(front.kind, SegmentKind::Text) && self.segments.len() > 1);
            if !closes {
                return;
            }
            out.push(StreamEvent::ContentBlockStop { index });
            self.segments.pop_front();
        }
    }

    fn stop_reason(&self) -> StopReason {
        match self.finish_reason.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ if self.tool_emitted => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        }
    }

    /// `usage` zerado é upstream que não contou, não resposta de custo zero.
    fn delta_usage(&self, usage: Option<Usage>) -> DeltaUsage {
        let usage = usage.unwrap_or_default();
        DeltaUsage {
            input_tokens: Some(usage.prompt_tokens).filter(|&t| t > 0),
            output_tokens: match usage.completion_tokens {
                0 => self.estimated_output_tokens(),
                reported => reported,
            },
        }
    }

    fn estimated_output_tokens(&self) -> u32 {
        let chars_per_token = self.meta.chars_per_token;
        if chars_per_token > 0.0 {
            (self.output_chars as f64 / chars_per_token).ceil() as u32
        } else {
            u32::try_from(self.output_chars).unwrap_or(u32::MAX)
        }
    }
}

fn set_if_empty(field: &mut String, value: Option<&str>) {
    if field.is_empty()
        && let Some(value) = value
    {
        value.clone_into(field);
    }
}

/// Dobra os eventos de uma resposta completa na mensagem não-stream, para os dois
/// modos não divergirem. Evento `error` vira `Err`, e stream que não chegou ao
/// `message_stop` também. Argumentos que não são objeto JSON viram `input: {}`.
pub fn collect_response(events: &[StreamEvent]) -> Result<MessagesResponse, ApiError> {
    let mut message: Option<MessagesResponse> = None;
    let mut args = String::new();
    let mut stopped = false;
    for event in events {
        match event {
            StreamEvent::Error { error } => return Err(error.clone()),
            StreamEvent::MessageStart { message: m } => message = Some(m.clone()),
            StreamEvent::MessageStop => stopped = true,
            StreamEvent::Ping => {}
            _ => {
                if let Some(m) = &mut message {
                    fold_event(m, &mut args, event);
                }
            }
        }
    }
    match message {
        Some(m) if stopped => Ok(m),
        _ => Err(ApiError {
            kind: "api_error".into(),
            message: "stream do upstream incompleto".into(),
        }),
    }
}

fn fold_event(message: &mut MessagesResponse, args: &mut String, event: &StreamEvent) {
    match event {
        StreamEvent::ContentBlockStart { content_block, .. } => {
            message.content.push(content_block.clone());
        }
        StreamEvent::ContentBlockDelta { delta, .. } => match (delta, message.content.last_mut()) {
            (BlockDelta::TextDelta { text }, Some(ContentBlock::Text { text: acc })) => {
                acc.push_str(text);
            }
            (BlockDelta::InputJsonDelta { partial_json }, _) => args.push_str(partial_json),
            _ => {}
        },
        StreamEvent::ContentBlockStop { .. } => {
            if let Some(ContentBlock::ToolUse { input, .. }) = message.content.last_mut() {
                let parsed = serde_json::from_str::<Value>(args).ok();
                *input = parsed
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::Object(Map::new()));
            }
            args.clear();
        }
        StreamEvent::MessageDelta { delta, usage } => {
            message.stop_reason = delta.stop_reason;
            message.stop_sequence.clone_from(&delta.stop_sequence);
            message.usage.output_tokens = usage.output_tokens;
            if let Some(input) = usage.input_tokens {
                message.usage.input_tokens = input;
            }
        }
        _ => {}
    }
}
