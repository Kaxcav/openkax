//! Aceite do tradutor (F1-1). Os testes de fio valem desde o CONTRACT; os de
//! tradução ficam com `#[ignore = "F1-…"]` até a tarefa de cada um (rode com
//! `cargo test -- --ignored` para ver vermelho).
//!
//! `claude_code_request.json` foi remontado a partir do formato público da API e
//! do que o Claude Code manda (system em blocos com `cache_control`, tools, tool de
//! servidor, `thinking`, `metadata`); não é captura de tráfego real. A captura é
//! parte do aceite manual (`docs/smoke/fase-1-aceite.md`, F1-8).

use serde_json::{Value, json};

use super::*;
use crate::scheduler::estimate_tokens;
use crate::translate::openai::{ChunkChoice, Delta};

const CLAUDE_CODE: &str =
    include_str!("../../../tests/fixtures/anthropic/claude_code_request.json");

fn parse(v: Value) -> MessagesRequest {
    serde_json::from_value(v).expect("request Anthropic válida")
}

/// Traduz `extra` + `messages` mínimos e devolve o JSON OpenAI.
fn translate(v: Value) -> Value {
    serde_json::to_value(to_openai_request(&parse(v))).unwrap()
}

fn with_messages(messages: Value) -> Value {
    translate(json!({ "model": "kax/x", "max_tokens": 10, "messages": messages }))
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

// ---------------------------------------------------------------- fio (F1-1)

#[test]
fn fixture_parses_and_tolerates_unknown_fields_and_server_tools() {
    let r: MessagesRequest = serde_json::from_str(CLAUDE_CODE).unwrap();
    assert_eq!(r.model, "kax/summarizer");
    assert_eq!(r.max_tokens, Some(32000));
    assert!(r.is_stream());
    assert_eq!(r.tools.len(), 3);
    assert_eq!(r.tools[2].input_schema, None);
    assert_eq!(
        r.tool_choice,
        Some(ToolChoice::Auto {
            disable_parallel_tool_use: false
        })
    );
    assert_eq!(r.messages.len(), 3);
    let MessageContent::Blocks(assistant) = &r.messages[1].content else {
        panic!("assistant em blocos");
    };
    assert!(matches!(assistant[0], ContentBlock::Thinking { .. }));
    assert!(matches!(&assistant[2], ContentBlock::ToolUse { id, .. } if id == "toolu_01A"));
}

#[test]
fn unknown_block_and_image_source_do_not_break_the_parse() {
    let r = parse(json!({
        "model": "m",
        "messages": [{ "role": "user", "content": [
            { "type": "document", "source": { "type": "base64", "data": "x" } },
            { "type": "image", "source": { "type": "file", "file_id": "f" } },
        ]}],
    }));
    let MessageContent::Blocks(b) = &r.messages[0].content else {
        panic!("blocos");
    };
    assert_eq!(b[0], ContentBlock::Unknown);
    assert_eq!(
        b[1],
        ContentBlock::Image {
            source: ImageSource::Unknown
        }
    );
}

/// Na Fase 1, quem barra formas estranhas é a tradução, não o parse.
#[test]
fn explicit_nulls_and_nameless_tools_parse() {
    let r = parse(json!({
        "model": "m",
        "stop_sequences": null,
        "tool_choice": { "type": "auto", "disable_parallel_tool_use": null },
        "messages": [{ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "t", "content": "ok", "is_error": null },
        ]}],
        "tools": [{ "type": "mcp_toolset", "mcp_server_name": "x" }],
    }));
    assert_eq!(r.tools[0].name, "");
    assert!(r.stop_sequences.is_empty());
    let n = parse(json!({ "model": "m", "tools": null, "messages": [] }));
    assert!(n.tools.is_empty());
}

#[test]
fn tool_choice_variants_parse() {
    for (wire, want) in [
        (
            json!({ "type": "auto" }),
            ToolChoice::Auto {
                disable_parallel_tool_use: false,
            },
        ),
        (
            json!({ "type": "any", "disable_parallel_tool_use": true }),
            ToolChoice::Any {
                disable_parallel_tool_use: true,
            },
        ),
        (
            json!({ "type": "tool", "name": "Read" }),
            ToolChoice::Tool {
                name: "Read".into(),
                disable_parallel_tool_use: false,
            },
        ),
        (json!({ "type": "none" }), ToolChoice::None),
    ] {
        assert_eq!(serde_json::from_value::<ToolChoice>(wire).unwrap(), want);
    }
}

#[test]
fn events_have_the_exact_wire_format() {
    let start = StreamEvent::MessageStart {
        message: MessagesResponse {
            id: "msg_1".into(),
            kind: MessageKind::Message,
            role: Role::Assistant,
            content: Vec::new(),
            model: "kax/summarizer".into(),
            stop_reason: None,
            stop_sequence: None,
            usage: MessageUsage {
                input_tokens: 12,
                output_tokens: 0,
            },
        },
    };
    let tool = StreamEvent::ContentBlockStart {
        index: 1,
        content_block: ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({}),
        },
    };
    let delta = StreamEvent::MessageDelta {
        delta: MessageDeltaBody {
            stop_reason: Some(StopReason::ToolUse),
            stop_sequence: None,
        },
        usage: DeltaUsage {
            input_tokens: None,
            output_tokens: 7,
        },
    };
    let error = StreamEvent::Error {
        error: ApiError {
            kind: "api_error".into(),
            message: "caiu".into(),
        },
    };
    for (event, want) in [
        (
            start,
            r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"kax/summarizer","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}

"#,
        ),
        (
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                },
            },
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#,
        ),
        (
            tool,
            r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"Read","input":{}}}

"#,
        ),
        (
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: BlockDelta::TextDelta { text: "oi".into() },
            },
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"oi"}}

"#,
        ),
        (
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: BlockDelta::InputJsonDelta {
                    partial_json: "{\"a\"".into(),
                },
            },
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}

"#,
        ),
        (
            StreamEvent::ContentBlockStop { index: 1 },
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":1}

"#,
        ),
        (
            delta,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":7}}

"#,
        ),
        (
            StreamEvent::MessageStop,
            r#"event: message_stop
data: {"type":"message_stop"}

"#,
        ),
        (
            StreamEvent::Ping,
            r#"event: ping
data: {"type":"ping"}

"#,
        ),
        (
            error,
            r#"event: error
data: {"type":"error","error":{"type":"api_error","message":"caiu"}}

"#,
        ),
    ] {
        assert_eq!(
            String::from_utf8(encode_sse(&event).to_vec()).unwrap(),
            want
        );
    }
}

#[test]
fn error_body_has_the_anthropic_shape() {
    assert_eq!(
        error_body("rate_limit_error", "sem saldo"),
        json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "sem saldo" } })
    );
}

// ------------------------------------------------------------- request (F1-2)

#[test]
#[ignore = "F1-2"]
fn system_string_and_blocks_become_one_system_message_without_cache_control() {
    let s = translate(json!({
        "model": "kax/x", "max_tokens": 1, "system": "regras",
        "messages": [user("oi")],
    }));
    assert_eq!(
        s["messages"],
        json!([{ "role": "system", "content": "regras" }, user("oi")])
    );

    let b = translate(json!({
        "model": "kax/x", "max_tokens": 1,
        "system": [
            { "type": "text", "text": "um", "cache_control": { "type": "ephemeral" } },
            { "type": "text", "text": "dois" },
        ],
        "messages": [user("oi")],
    }));
    assert_eq!(
        b["messages"][0],
        json!({ "role": "system", "content": "um\n\ndois" })
    );

    let none = translate(json!({ "model": "kax/x", "max_tokens": 1, "messages": [user("oi")] }));
    assert_eq!(none["messages"], json!([user("oi")]));
}

#[test]
#[ignore = "F1-2"]
fn text_blocks_join_into_a_string() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "text", "text": "a", "cache_control": { "type": "ephemeral" } },
        { "type": "text", "text": "b" },
    ]}]));
    assert_eq!(
        v["messages"],
        json!([{ "role": "user", "content": "a\n\nb" }])
    );
}

#[test]
#[ignore = "F1-2"]
fn images_become_image_url_parts_in_order() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "text", "text": "veja" },
        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } },
        { "type": "image", "source": { "type": "url", "url": "https://x/y.png" } },
        { "type": "image", "source": { "type": "file", "file_id": "f" } },
    ]}]));
    assert_eq!(
        v["messages"],
        json!([{ "role": "user", "content": [
            { "type": "text", "text": "veja" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
            { "type": "image_url", "image_url": { "url": "https://x/y.png" } },
        ]}])
    );
}

#[test]
#[ignore = "F1-2"]
fn tool_use_becomes_assistant_tool_calls() {
    let v = with_messages(json!([
        { "role": "assistant", "content": [
            { "type": "text", "text": "vou ler" },
            { "type": "tool_use", "id": "t1", "name": "Read", "input": { "p": "a" } },
            { "type": "tool_use", "id": "t2", "name": "Ls", "input": {} },
        ]},
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": "t3", "name": "Ls", "input": {} },
        ]},
    ]));
    let call = |id: &str, name: &str, args: &str| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args } });
    assert_eq!(
        v["messages"],
        json!([
            {
                "role": "assistant",
                "content": "vou ler",
                "tool_calls": [call("t1", "Read", r#"{"p":"a"}"#), call("t2", "Ls", "{}")],
            },
            { "role": "assistant", "content": null, "tool_calls": [call("t3", "Ls", "{}")] },
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn tool_results_become_tool_messages_before_the_user_text() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "text", "text": "antes" },
        { "type": "tool_result", "tool_use_id": "t1", "content": "texto" },
        { "type": "tool_result", "tool_use_id": "t2", "content": [
            { "type": "text", "text": "um", "cache_control": { "type": "ephemeral" } },
            { "type": "text", "text": "dois" },
        ]},
        { "type": "tool_result", "tool_use_id": "t3" },
        { "type": "text", "text": "depois" },
    ]}]));
    assert_eq!(
        v["messages"],
        json!([
            { "role": "tool", "tool_call_id": "t1", "content": "texto" },
            { "role": "tool", "tool_call_id": "t2", "content": "um\ndois" },
            { "role": "tool", "tool_call_id": "t3", "content": "" },
            { "role": "user", "content": "antes\n\ndepois" },
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn tool_result_is_error_is_kept_in_the_text() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "tool_result", "tool_use_id": "t1", "content": "sem arquivo", "is_error": true },
    ]}]));
    assert_eq!(
        v["messages"][0]["content"].as_str().unwrap(),
        format!("{TOOL_ERROR_PREFIX}sem arquivo")
    );
}

/// A mensagem `tool` do OpenAI só leva texto: a imagem vai numa mensagem `user`
/// logo depois dos resultados.
#[test]
#[ignore = "F1-2"]
fn image_inside_tool_result_moves_to_a_user_message() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "tool_result", "tool_use_id": "t1", "content": [
            { "type": "text", "text": "print" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } },
        ]},
    ]}]));
    assert_eq!(
        v["messages"],
        json!([
            { "role": "tool", "tool_call_id": "t1", "content": "print" },
            { "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
            ]},
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn tools_become_functions_and_server_tools_are_dropped() {
    let v = translate(json!({
        "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
        "tools": [
            { "name": "Read", "description": "lê", "input_schema": { "type": "object" }, "cache_control": { "type": "ephemeral" } },
            { "name": "Ls", "input_schema": { "type": "object", "properties": {} } },
            { "type": "web_search_20250305", "name": "web_search" },
        ],
    }));
    assert_eq!(
        v["tools"],
        json!([
            { "type": "function", "function": { "name": "Read", "description": "lê", "parameters": { "type": "object" } } },
            { "type": "function", "function": { "name": "Ls", "parameters": { "type": "object", "properties": {} } } },
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn tool_choice_table() {
    let tools = json!([{ "name": "Read", "input_schema": { "type": "object" } }]);
    for (choice, want) in [
        (json!({ "type": "auto" }), json!("auto")),
        (json!({ "type": "any" }), json!("required")),
        (
            json!({ "type": "tool", "name": "Read" }),
            json!({ "type": "function", "function": { "name": "Read" } }),
        ),
        (json!({ "type": "none" }), json!("none")),
    ] {
        let v = translate(json!({
            "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
            "tools": tools, "tool_choice": choice,
        }));
        assert_eq!(v["tool_choice"], want, "{choice}");
        assert!(v.get("parallel_tool_calls").is_none(), "{choice}");
    }
}

#[test]
#[ignore = "F1-2"]
fn disable_parallel_tool_use_maps_to_parallel_tool_calls_false() {
    let v = translate(json!({
        "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
        "tools": [{ "name": "Read", "input_schema": { "type": "object" } }],
        "tool_choice": { "type": "auto", "disable_parallel_tool_use": true },
    }));
    assert_eq!(v["tool_choice"], "auto");
    assert_eq!(v["parallel_tool_calls"], false);
}

/// OpenAI recusa `tool_choice` sem `tools`; server tools sozinhas contam como sem tools.
#[test]
#[ignore = "F1-2"]
fn tool_choice_without_usable_tools_is_omitted() {
    for tools in [
        json!([]),
        json!([{ "type": "web_search_20250305", "name": "web_search" }]),
    ] {
        let v = translate(json!({
            "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
            "tools": tools, "tool_choice": { "type": "any" },
        }));
        assert!(v.get("tools").is_none(), "{v}");
        assert!(v.get("tool_choice").is_none(), "{v}");
    }
}

/// Mensagem `user` que fica vazia não é enviada (conteúdo `[]` dá 400 no upstream).
#[test]
#[ignore = "F1-2"]
fn user_message_left_empty_is_omitted() {
    let v = with_messages(json!([
        { "role": "user", "content": [{ "type": "document", "source": { "type": "base64", "data": "x" } }] },
        user("oi"),
    ]));
    assert_eq!(v["messages"], json!([user("oi")]));
}

/// A assinatura vem dentro do `tool_use.id` e volta para onde o Gemini espera; o
/// `id` limpo vale nas duas pontas (ADR 0006). Só a 1ª chamada paralela tem.
#[test]
#[ignore = "F1-2"]
fn signature_in_the_id_goes_back_to_extra_content() {
    let signed = format!("c1{TOOL_ID_SIGNATURE_SEP}{}", "5349472b2f3d"); // "SIG+/="
    assert_eq!(
        decode_tool_use_id(&signed),
        ("c1".to_owned(), Some("SIG+/=".to_owned()))
    );
    assert_eq!(
        decode_tool_use_id("toolu_01"),
        ("toolu_01".to_owned(), None)
    );
    assert_eq!(
        decode_tool_use_id(&format!("c1{TOOL_ID_SIGNATURE_SEP}nao-hex")),
        (format!("c1{TOOL_ID_SIGNATURE_SEP}nao-hex"), None),
        "sufixo que não é nosso fica como veio"
    );

    let v = with_messages(json!([
        { "role": "assistant", "content": [
            { "type": "tool_use", "id": signed, "name": "Read", "input": {} },
            { "type": "tool_use", "id": "c2", "name": "Ls", "input": {} },
        ]},
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": signed, "content": "um" },
            { "type": "tool_result", "tool_use_id": "c2", "content": "dois" },
        ]},
    ]));
    assert_eq!(
        v["messages"],
        json!([
            { "role": "assistant", "content": null, "tool_calls": [
                {
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "Read", "arguments": "{}" },
                    "extra_content": { "google": { "thought_signature": "SIG+/=" } },
                },
                { "id": "c2", "type": "function", "function": { "name": "Ls", "arguments": "{}" } },
            ]},
            { "role": "tool", "tool_call_id": "c1", "content": "um" },
            { "role": "tool", "tool_call_id": "c2", "content": "dois" },
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn thinking_and_unknown_blocks_are_dropped() {
    let v = with_messages(json!([
        user("oi"),
        { "role": "assistant", "content": [{ "type": "thinking", "thinking": "hmm", "signature": "s" }] },
        { "role": "assistant", "content": [
            { "type": "redacted_thinking", "data": "x" },
            { "type": "text", "text": "fim" },
        ]},
        { "role": "user", "content": [
            { "type": "document", "source": { "type": "base64", "data": "x" } },
            { "type": "text", "text": "ok" },
        ]},
    ]));
    assert_eq!(
        v["messages"],
        json!([
            user("oi"),
            { "role": "assistant", "content": "fim" },
            { "role": "user", "content": "ok" },
        ])
    );
}

#[test]
#[ignore = "F1-2"]
fn scalar_params_map_and_the_rest_is_dropped() {
    let v = translate(json!({
        "model": "kax/x",
        "max_tokens": 321,
        "stream": true,
        "temperature": 0.5,
        "top_p": 0.9,
        "top_k": 40,
        "stop_sequences": ["FIM", "END"],
        "metadata": { "user_id": "u" },
        "thinking": { "type": "enabled", "budget_tokens": 10 },
        "messages": [user("oi")],
    }));
    assert_eq!(
        v,
        json!({
            "model": "kax/x",
            "max_tokens": 321,
            "stream": true,
            "temperature": 0.5,
            "top_p": 0.9,
            "stop": ["FIM", "END"],
            "messages": [user("oi")],
        })
    );
    let none = with_messages(json!([user("oi")]));
    assert!(none.get("stop").is_none(), "{none}");
    assert!(none.get("temperature").is_none(), "{none}");
}

#[test]
#[ignore = "F1-2"]
fn claude_code_fixture_translates_whole() {
    let mut raw: Value = serde_json::from_str(CLAUDE_CODE).unwrap();
    // System grande (> 6k tokens), como o do Claude Code.
    raw["system"][1]["text"] = "regra de engenharia. ".repeat(1500).into();
    let v = serde_json::to_value(to_openai_request(&parse(raw))).unwrap();

    let text = v.to_string();
    for gone in [
        "cache_control",
        "thinking",
        "signature",
        "metadata",
        "context_management",
        "web_search",
    ] {
        assert!(!text.contains(gone), "{gone} vazou para o upstream");
    }
    let msgs = v["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "tool", "user"]
    );
    assert!(
        msgs[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are Claude Code")
    );
    assert!(msgs[0]["content"].as_str().unwrap().len() > 6000 * 4);
    assert_eq!(msgs[2]["content"], "Vou procurar.");
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "toolu_01A");
    assert_eq!(msgs[2]["tool_calls"][1]["function"]["name"], "Read");
    assert_eq!(msgs[3]["tool_call_id"], "toolu_01A");
    assert_eq!(msgs[4]["content"], "pub fn reserve(&self) {}");
    assert_eq!(msgs[5]["content"], "Resuma em uma frase.");
    assert_eq!(v["tools"].as_array().unwrap().len(), 2);
    assert_eq!(v["tool_choice"], "auto");
    assert_eq!(v["max_tokens"], 32000);
    assert_eq!(v["stream"], true);

    // O tipo OpenAI que sai é o que o scheduler estima.
    let req: ChatCompletionRequest = serde_json::from_value(v).unwrap();
    assert!(estimate_tokens(&req, 4.0) > 6000);
}

// -------------------------------------------------------------- stream (F1-3)

fn meta() -> StreamMeta {
    StreamMeta {
        message_id: "msg_1".into(),
        model: "kax/summarizer".into(),
        est_input_tokens: 12,
        chars_per_token: 4.0,
    }
}

fn text_chunk(text: &str) -> ChatCompletionChunk {
    delta_chunk(
        0,
        Delta {
            content: Some(text.into()),
            ..Delta::default()
        },
        None,
    )
}

fn tool_chunk(calls: Value) -> ChatCompletionChunk {
    delta_chunk(
        0,
        Delta {
            tool_calls: serde_json::from_value(calls).unwrap(),
            ..Delta::default()
        },
        None,
    )
}

fn finish_chunk(reason: &str) -> ChatCompletionChunk {
    delta_chunk(0, Delta::default(), Some(reason))
}

fn delta_chunk(index: u32, delta: Delta, finish: Option<&str>) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: "chatcmpl-1".into(),
        object: "chat.completion.chunk".into(),
        created: 1,
        model: "upstream".into(),
        choices: vec![ChunkChoice {
            index,
            delta,
            finish_reason: finish.map(str::to_owned),
        }],
        usage: None,
        extra: Default::default(),
    }
}

/// Roda chunks + `finish` e devolve os eventos.
fn run(chunks: &[ChatCompletionChunk], usage: Option<Usage>) -> Vec<StreamEvent> {
    let mut t = OpenAiToAnthropicStream::new(meta());
    let mut events: Vec<StreamEvent> = chunks.iter().flat_map(|c| t.push(c)).collect();
    events.extend(t.finish(usage));
    events
}

fn wire(events: &[StreamEvent]) -> String {
    events
        .iter()
        .map(|e| String::from_utf8(encode_sse(e).to_vec()).unwrap())
        .collect()
}

fn names(events: &[StreamEvent]) -> Vec<&'static str> {
    events.iter().map(StreamEvent::name).collect()
}

fn stop_reason(events: &[StreamEvent]) -> Option<StopReason> {
    events.iter().find_map(|e| match e {
        StreamEvent::MessageDelta { delta, .. } => delta.stop_reason,
        _ => None,
    })
}

const USAGE: Usage = Usage {
    prompt_tokens: 20,
    completion_tokens: 5,
    total_tokens: 25,
};

#[test]
#[ignore = "F1-3"]
fn text_only_stream_byte_by_byte() {
    let events = run(
        &[
            text_chunk("Ola"),
            text_chunk(" mundo"),
            finish_chunk("stop"),
        ],
        Some(USAGE),
    );
    assert_eq!(
        wire(&events),
        concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"kax/summarizer","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Ola"}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" mundo"}}"#,
            "\n\n",
            "event: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":20,"output_tokens":5}}"#,
            "\n\n",
            "event: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        )
    );
}

/// Cada `push` devolve o que já pode sair: o stream não espera o fim.
#[test]
#[ignore = "F1-3"]
fn each_push_emits_what_is_ready() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    assert_eq!(
        names(&t.push(&text_chunk("a"))),
        [
            "message_start",
            "content_block_start",
            "content_block_delta"
        ]
    );
    assert_eq!(names(&t.push(&text_chunk("b"))), ["content_block_delta"]);
    assert_eq!(
        names(&t.push(&tool_chunk(
            json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{" } }])
        ))),
        [
            "content_block_stop",
            "content_block_start",
            "content_block_delta"
        ],
        "a 1ª tool fecha o texto aberto"
    );
    assert_eq!(
        names(&t.push(&tool_chunk(
            json!([{ "index": 1, "id": "c2", "function": { "name": "Ls", "arguments": "{" } }])
        ))),
        Vec::<&str>::new(),
        "a 2ª tool fica retida"
    );
    assert_eq!(
        names(&t.push(&tool_chunk(
            json!([{ "index": 0, "function": { "arguments": "}" } }])
        ))),
        ["content_block_delta"],
        "a aberta segue ao vivo"
    );
}

/// O bloco aberto segue ao vivo; o resto fica retido até ele fechar, na ordem em
/// que apareceu: quem lê espera um bloco aberto por vez.
#[test]
#[ignore = "F1-3"]
fn text_and_two_interleaved_tools_byte_by_byte() {
    let events = run(
        &[
            text_chunk("Vou ler"),
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "type": "function", "function": { "name": "Read", "arguments": "{\"p\"" } }]),
            ),
            tool_chunk(
                json!([{ "index": 1, "id": "c2", "type": "function", "function": { "name": "Grep", "arguments": "{\"q\"" } }]),
            ),
            tool_chunk(json!([{ "index": 0, "function": { "arguments": ":1}" } }])),
            tool_chunk(json!([{ "index": 1, "function": { "arguments": ":2}" } }])),
            finish_chunk("tool_calls"),
        ],
        Some(USAGE),
    );
    assert_eq!(
        wire(&events),
        concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"kax/summarizer","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Vou ler"}}"#,
            "\n\n",
            "event: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c1","name":"Read","input":{}}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"p\""}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":1}"}}"#,
            "\n\n",
            "event: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"c2","name":"Grep","input":{}}}"#,
            "\n\n",
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":2}"}}"#,
            "\n\n",
            "event: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":2}"#,
            "\n\n",
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":20,"output_tokens":5}}"#,
            "\n\n",
            "event: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        )
    );
}

#[test]
#[ignore = "F1-3"]
fn text_arriving_while_a_tool_is_open_waits_for_it() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{" } }]),
            ),
            text_chunk("depois"),
            tool_chunk(json!([{ "index": 0, "function": { "arguments": "}" } }])),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    let got = collect_response(&events).unwrap();
    assert_eq!(
        got.content,
        vec![
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Read".into(),
                input: json!({})
            },
            ContentBlock::Text {
                text: "depois".into()
            },
        ]
    );
}

#[test]
#[ignore = "F1-3"]
fn stop_reason_table() {
    let tool = tool_chunk(
        json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": "{}" } }]),
    );
    for (finish, want) in [
        ("stop", StopReason::EndTurn),
        ("length", StopReason::MaxTokens),
        ("content_filter", StopReason::Refusal),
        ("qualquer_outro", StopReason::EndTurn),
    ] {
        let e = run(&[text_chunk("a"), finish_chunk(finish)], None);
        assert_eq!(stop_reason(&e), Some(want), "{finish}");
    }
    let e = run(&[tool.clone(), finish_chunk("tool_calls")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::ToolUse));
    // Sem `finish_reason` (EOF limpo do upstream).
    let e = run(&[text_chunk("a")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::EndTurn));
    // Há provider que manda `stop` com tool_calls; o cliente precisa de `tool_use`.
    let e = run(&[tool.clone(), finish_chunk("stop")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::ToolUse));
    // Sem `finish_reason` ou com valor fora do padrão, tool emitida também vale.
    let e = run(std::slice::from_ref(&tool), None);
    assert_eq!(stop_reason(&e), Some(StopReason::ToolUse));
    let e = run(&[tool.clone(), finish_chunk("qualquer_outro")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::ToolUse));
    // Truncado continua truncado, com tool ou sem; filtro também.
    let e = run(&[tool.clone(), finish_chunk("length")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::MaxTokens));
    let e = run(&[tool, finish_chunk("content_filter")], None);
    assert_eq!(stop_reason(&e), Some(StopReason::Refusal));
}

/// Há upstream que sinaliza falha com `finish_reason: "error"`: não é sucesso.
#[test]
#[ignore = "F1-3"]
fn finish_reason_error_becomes_error_event() {
    let e = run(&[text_chunk("a"), finish_chunk("error")], None);
    assert_eq!(
        names(&e),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error"
        ]
    );
}

#[test]
#[ignore = "F1-3"]
fn empty_text_other_choices_and_reasoning_open_no_block() {
    let mut reasoning = text_chunk("");
    reasoning.choices[0].delta.content = None;
    reasoning.choices[0]
        .delta
        .extra
        .insert("reasoning_content".into(), "hmm".into());
    let other_choice = delta_chunk(
        1,
        Delta {
            content: Some("x".into()),
            ..Delta::default()
        },
        None,
    );
    let role = delta_chunk(
        0,
        Delta {
            role: Some("assistant".into()),
            ..Delta::default()
        },
        None,
    );
    let e = run(
        &[
            role,
            text_chunk(""),
            reasoning,
            other_choice,
            finish_chunk("stop"),
        ],
        None,
    );
    assert_eq!(
        names(&e),
        ["message_start", "message_delta", "message_stop"]
    );
}

#[test]
#[ignore = "F1-3"]
fn tool_without_args_has_no_delta_and_name_may_arrive_late() {
    let e = run(
        &[
            tool_chunk(json!([{ "index": 0, "id": "c1", "function": { "arguments": "" } }])),
            tool_chunk(json!([{ "index": 0, "function": { "name": "Ls" } }])),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert_eq!(
        names(&e),
        [
            "message_start",
            "content_block_start",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(
        e[1],
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Ls".into(),
                input: json!({})
            },
        }
    );
}

/// O bloco só abre com `id` e `name`; os argumentos esperam. Sem `id` nenhum até
/// o fim, o gateway gera um, diferente por chamada.
#[test]
#[ignore = "F1-3"]
fn tool_id_may_arrive_late_or_never() {
    let e = run(
        &[
            tool_chunk(json!([{ "index": 0, "function": { "arguments": "{\"a\":" } }])),
            tool_chunk(json!([{ "index": 0, "function": { "name": "Read" } }])),
            tool_chunk(json!([{ "index": 0, "id": "c1", "function": { "arguments": "1}" } }])),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert_eq!(
        collect_response(&e).unwrap().content,
        vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({ "a": 1 })
        }]
    );

    let e = run(
        &[
            tool_chunk(json!([{ "index": 0, "function": { "name": "Read", "arguments": "{}" } }])),
            tool_chunk(json!([{ "index": 1, "function": { "name": "Ls", "arguments": "{}" } }])),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    let content = collect_response(&e).unwrap().content;
    let ids: Vec<&str> = content
        .iter()
        .map(|b| match b {
            ContentBlock::ToolUse { id, .. } => id.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, ["toolu_msg_1_0", "toolu_msg_1_1"]);
}

/// Gemini 3+ devolve 400 sem a assinatura na rodada seguinte; o `tool_use.id` a
/// leva, em hexadecimal, até o cliente e de volta (ADR 0006).
#[test]
#[ignore = "F1-3"]
fn thought_signature_travels_in_the_tool_use_id() {
    let e = run(
        &[
            tool_chunk(json!([{
                "index": 0,
                "id": "c1",
                "type": "function",
                "function": { "name": "Read", "arguments": "{}" },
                "extra_content": { "google": { "thought_signature": "SIG+/=" } },
            }])),
            tool_chunk(
                json!([{ "index": 1, "id": "c2", "function": { "name": "Ls", "arguments": "{}" } }]),
            ),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    let content = collect_response(&e).unwrap().content;
    let ContentBlock::ToolUse { id: first, .. } = &content[0] else {
        panic!("{content:?}");
    };
    assert_eq!(first, &format!("c1{TOOL_ID_SIGNATURE_SEP}5349472b2f3d"));
    assert_eq!(first, &encode_tool_use_id("c1", Some("SIG+/=")));
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
    let ContentBlock::ToolUse { id: second, .. } = &content[1] else {
        panic!("{content:?}");
    };
    assert_eq!(second, "c2");
}

/// Sem `usage` do upstream: saída estimada por `chars_per_token`; a entrada
/// estimada fica a de `message_start` (o delta não a repete).
#[test]
#[ignore = "F1-3"]
fn missing_usage_estimates_output_tokens() {
    let e = run(
        &[
            text_chunk("abcdefgh"),
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": "{\"a\":1}" } }]),
            ),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    let usage = e.iter().find_map(|ev| match ev {
        StreamEvent::MessageDelta { usage, .. } => Some(*usage),
        _ => None,
    });
    // 8 + 7 chars / 4 = 3,75 → 4.
    assert_eq!(
        usage,
        Some(DeltaUsage {
            input_tokens: None,
            output_tokens: 4
        })
    );
}

#[test]
#[ignore = "F1-3"]
fn mid_stream_failure_ends_with_error_and_nothing_after() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    let mut e = t.push(&text_chunk("oi"));
    e.extend(t.fail("api_error", "o provider caiu"));
    assert_eq!(
        names(&e),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error"
        ]
    );
    assert_eq!(
        e.last(),
        Some(&StreamEvent::Error {
            error: ApiError {
                kind: "api_error".into(),
                message: "o provider caiu".into()
            },
        })
    );
    assert!(t.push(&text_chunk("zumbi")).is_empty());
    assert!(t.finish(None).is_empty());
    assert!(t.fail("api_error", "de novo").is_empty());
    assert!(matches!(collect_response(&e), Err(ApiError { kind, .. }) if kind == "api_error"));
}

#[test]
#[ignore = "F1-3"]
fn finish_is_idempotent_and_empty_upstream_still_closes_the_message() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    let e = t.finish(None);
    assert_eq!(
        names(&e),
        ["message_start", "message_delta", "message_stop"]
    );
    assert!(t.finish(None).is_empty());
}

#[test]
#[ignore = "F1-3"]
fn collect_builds_the_full_response() {
    let events = run(
        &[
            text_chunk("Vou "),
            text_chunk("ler"),
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{\"p\":" } }]),
            ),
            tool_chunk(
                json!([{ "index": 1, "id": "c2", "function": { "name": "Ls", "arguments": "" } }]),
            ),
            tool_chunk(json!([{ "index": 0, "function": { "arguments": "1}" } }])),
            finish_chunk("tool_calls"),
        ],
        Some(USAGE),
    );
    assert_eq!(
        serde_json::to_value(collect_response(&events).unwrap()).unwrap(),
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "Vou ler" },
                { "type": "tool_use", "id": "c1", "name": "Read", "input": { "p": 1 } },
                { "type": "tool_use", "id": "c2", "name": "Ls", "input": {} },
            ],
            "model": "kax/summarizer",
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": { "input_tokens": 20, "output_tokens": 5 },
        })
    );
}

/// Argumentos truncados por `max_tokens` não são JSON: `input` fica `{}` e o
/// `stop_reason` conta o que houve.
#[test]
#[ignore = "F1-3"]
fn collect_with_truncated_args_keeps_empty_input() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{\"p\":\"meio" } }]),
            ),
            finish_chunk("length"),
        ],
        None,
    );
    let r = collect_response(&events).unwrap();
    assert_eq!(r.stop_reason, Some(StopReason::MaxTokens));
    assert_eq!(
        r.content,
        vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({})
        }]
    );
    assert_eq!(
        r.usage.input_tokens, 12,
        "sem usage do upstream fica a estimativa"
    );
}

/// `input` é sempre objeto: JSON válido de outro tipo também vira `{}`.
#[test]
#[ignore = "F1-3"]
fn collect_with_non_object_args_keeps_empty_input() {
    for args in ["null", "[1]", "3"] {
        let events = run(
            &[
                tool_chunk(
                    json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": args } }]),
                ),
                finish_chunk("tool_calls"),
            ],
            None,
        );
        assert_eq!(
            collect_response(&events).unwrap().content,
            vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Ls".into(),
                input: json!({})
            }],
            "{args}"
        );
    }
}
