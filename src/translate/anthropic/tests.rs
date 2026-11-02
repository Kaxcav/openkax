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
use crate::translate::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChunkChoice, Delta, Usage,
};

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
fn tool_use_becomes_assistant_tool_calls() {
    let v = with_messages(json!([
        { "role": "assistant", "content": [
            { "type": "text", "text": "vou ler" },
            { "type": "tool_use", "id": "t1", "name": "Read", "input": { "p": "a" } },
            { "type": "tool_use", "id": "t2", "name": "Ls", "input": {} },
        ]},
        user("segue"),
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
            user("segue"),
            { "role": "assistant", "content": null, "tool_calls": [call("t3", "Ls", "{}")] },
        ])
    );
}

#[test]
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
fn disable_parallel_tool_use_maps_to_parallel_tool_calls_false() {
    for (choice, want) in [
        (
            json!({ "type": "auto", "disable_parallel_tool_use": true }),
            json!("auto"),
        ),
        (
            json!({ "type": "any", "disable_parallel_tool_use": true }),
            json!("required"),
        ),
        (
            json!({ "type": "tool", "name": "Read", "disable_parallel_tool_use": true }),
            json!({ "type": "function", "function": { "name": "Read" } }),
        ),
    ] {
        let v = translate(json!({
            "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
            "tools": [{ "name": "Read", "input_schema": { "type": "object" } }],
            "tool_choice": choice,
        }));
        assert_eq!(v["tool_choice"], want, "{choice}");
        assert_eq!(v["parallel_tool_calls"], false, "{choice}");
    }
}

/// OpenAI recusa `tool_choice` sem `tools`; server tools sozinhas contam como sem tools.
#[test]
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

/// Imagem de `tool_result` e texto do usuário na mesma mensagem: uma `user` só,
/// sem duas seguidas, com a imagem da tool antes do que o usuário escreveu.
#[test]
fn tool_result_image_and_user_content_share_one_user_message() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "text", "text": "analise" },
        { "type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": [
            { "type": "image", "source": { "type": "url", "url": "https://x/tool.png" } },
        ]},
        { "type": "image", "source": { "type": "url", "url": "https://x/user.png" } },
    ]}]));
    assert_eq!(
        v["messages"],
        json!([
            { "role": "tool", "tool_call_id": "t1", "content": TOOL_ERROR_PREFIX },
            { "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "https://x/tool.png" } },
                { "type": "text", "text": "analise" },
                { "type": "image_url", "image_url": { "url": "https://x/user.png" } },
            ]},
        ])
    );
}

/// `tool_choice` forçando uma tool que não virou function daria 400 no upstream. O
/// pedido de não paralelizar continua valendo para as tools que sobraram.
#[test]
fn tool_choice_naming_a_dropped_tool_is_omitted() {
    let v = translate(json!({
        "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
        "tools": [
            { "name": "Read", "input_schema": { "type": "object" } },
            { "type": "web_search_20250305", "name": "web_search" },
        ],
        "tool_choice": { "type": "tool", "name": "web_search", "disable_parallel_tool_use": true },
    }));
    assert_eq!(v["tools"].as_array().unwrap().len(), 1);
    assert!(v.get("tool_choice").is_none(), "{v}");
    assert_eq!(v["parallel_tool_calls"], false, "{v}");
}

/// O OpenAI exige `tool` logo depois de `tool_calls`; a Anthropic funde turnos
/// seguidos do mesmo papel, então os dois assistants viram um só.
#[test]
fn consecutive_assistants_merge_before_the_tool_results() {
    let call = |id: &str| json!({ "id": id, "type": "function", "function": { "name": "Ls", "arguments": "{}" } });
    let tool = |id: &str| json!({ "role": "tool", "tool_call_id": id, "content": "r" });
    let result = |id: &str| json!({ "type": "tool_result", "tool_use_id": id, "content": "r" });
    let use_ = |id: &str| json!({ "type": "tool_use", "id": id, "name": "Ls", "input": {} });

    let two_calls = with_messages(json!([
        { "role": "assistant", "content": [{ "type": "text", "text": "um" }, use_("a")] },
        { "role": "assistant", "content": [{ "type": "text", "text": "dois" }, use_("b")] },
        { "role": "user", "content": [result("a"), result("b")] },
    ]));
    assert_eq!(
        two_calls["messages"],
        json!([
            { "role": "assistant", "content": "um\n\ndois", "tool_calls": [call("a"), call("b")] },
            tool("a"),
            tool("b"),
        ])
    );

    let text_after_call = with_messages(json!([
        { "role": "assistant", "content": [use_("a")] },
        { "role": "assistant", "content": "depois" },
        { "role": "user", "content": [result("a")] },
    ]));
    assert_eq!(
        text_after_call["messages"],
        json!([
            { "role": "assistant", "content": "depois", "tool_calls": [call("a")] },
            tool("a"),
        ])
    );

    let only_texts = with_messages(json!([
        { "role": "assistant", "content": "a" },
        { "role": "assistant", "content": [{ "type": "text", "text": "b" }] },
    ]));
    assert_eq!(
        only_texts["messages"],
        json!([{ "role": "assistant", "content": "a\n\nb" }])
    );
}

/// Blocos de texto do mesmo assistant não se colam.
#[test]
fn assistant_text_blocks_join_with_a_blank_line() {
    let v = with_messages(json!([{ "role": "assistant", "content": [
        { "type": "text", "text": "a" },
        { "type": "text", "text": "b" },
    ]}]));
    assert_eq!(v["messages"][0]["content"], "a\n\nb");
}

#[test]
fn empty_text_is_not_a_part_next_to_an_image() {
    let v = with_messages(json!([{ "role": "user", "content": [
        { "type": "text", "text": "" },
        { "type": "image", "source": { "type": "url", "url": "https://x/y.png" } },
    ]}]));
    assert_eq!(
        v["messages"][0]["content"],
        json!([{ "type": "image_url", "image_url": { "url": "https://x/y.png" } }])
    );
}

#[test]
fn tool_with_schema_but_no_name_is_dropped() {
    let v = translate(json!({
        "model": "kax/x", "max_tokens": 1, "messages": [user("oi")],
        "tools": [{ "input_schema": { "type": "object" } }],
    }));
    assert!(v.get("tools").is_none(), "{v}");
}

#[test]
fn empty_text_and_empty_system_are_not_sent() {
    let v = translate(json!({
        "model": "kax/x", "max_tokens": 1, "system": [{ "type": "text", "text": "" }],
        "messages": [
            user(""),
            { "role": "assistant", "content": "" },
            { "role": "user", "content": [
                { "type": "text", "text": "" },
                { "type": "text", "text": "oi" },
            ]},
        ],
    }));
    assert_eq!(v["messages"], json!([user("oi")]));
    let s = translate(
        json!({ "model": "kax/x", "max_tokens": 1, "system": "", "messages": [user("oi")] }),
    );
    assert_eq!(s["messages"], json!([user("oi")]));
}

/// `input: null` geraria `arguments: "null"`, que o upstream não aceita como objeto.
#[test]
fn null_tool_input_becomes_empty_arguments() {
    let v = with_messages(json!([{ "role": "assistant", "content": [
        { "type": "tool_use", "id": "t1", "name": "Ls", "input": null },
    ]}]));
    assert_eq!(
        v["messages"][0]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
}

/// ADR 0006: a assinatura não pesa na estimativa. O teste do scheduler cobre o
/// `extra_content`; este cobre o caminho inteiro a partir do `tool_use.id`.
#[test]
fn signature_does_not_count_in_the_estimate() {
    let conversation = |id: String| {
        parse(json!({
            "model": "kax/x", "max_tokens": 1,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": id, "name": "Ls", "input": {} },
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": id, "content": "r" },
                ]},
            ],
        }))
    };
    let signed = format!("c1{TOOL_ID_SIGNATURE_SEP}{}", "53".repeat(4000));
    assert_eq!(
        estimate_tokens(&to_openai_request(&conversation(signed)), 4.0),
        estimate_tokens(&to_openai_request(&conversation("c1".to_owned())), 4.0)
    );
}

#[test]
fn malformed_signature_suffix_stays_in_the_id() {
    for id in [
        format!("c1{TOOL_ID_SIGNATURE_SEP}"),
        format!("c1{TOOL_ID_SIGNATURE_SEP}abc"),
        format!("c1{TOOL_ID_SIGNATURE_SEP}ff"), // hexadecimal, mas não é UTF-8
        format!("{TOOL_ID_SIGNATURE_SEP}5349"),
    ] {
        assert_eq!(decode_tool_use_id(&id), (id.clone(), None));
    }
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

/// Tudo que sai tem de ser um stream que o cliente aceita: um `message_start`,
/// blocos em sequência e um aberto por vez, delta só no bloco aberto, e o fim
/// (`message_delta` + `message_stop`, ou `error`) só depois dos blocos fechados.
fn assert_well_formed(events: &[StreamEvent]) {
    let mut open = None;
    let mut next = 0;
    let mut stopped = false;
    assert!(matches!(
        events.first(),
        Some(StreamEvent::MessageStart { .. })
    ));
    for e in &events[1..] {
        assert!(!stopped, "evento depois do message_stop: {e:?}");
        match e {
            StreamEvent::MessageStart { .. } => panic!("2º message_start"),
            StreamEvent::ContentBlockStart { index, .. } => {
                assert_eq!(open, None, "{e:?}");
                assert_eq!(*index, next, "{e:?}");
                open = Some(*index);
                next += 1;
            }
            StreamEvent::ContentBlockDelta { index, .. } => {
                assert_eq!(open, Some(*index), "{e:?}");
            }
            StreamEvent::ContentBlockStop { index } => {
                assert_eq!(open, Some(*index), "{e:?}");
                open = None;
            }
            StreamEvent::MessageDelta { .. } => assert_eq!(open, None, "{e:?}"),
            StreamEvent::MessageStop => stopped = true,
            StreamEvent::Ping | StreamEvent::Error { .. } => {}
        }
    }
    assert!(stopped, "sem message_stop: {events:?}");
}

/// O id, o nome e os argumentos chegam em qualquer ordem, intercalados com texto:
/// o stream continua bem formado e nada se perde nem troca de lugar.
#[test]
fn any_interleaving_stays_well_formed_and_in_order() {
    let chunks = [
        text_chunk("a"),
        tool_chunk(json!([{ "index": 0, "function": { "arguments": "{\"x\":" } }])),
        text_chunk("b"),
        tool_chunk(
            json!([{ "index": 1, "id": "c2", "function": { "name": "Ls", "arguments": "{" } }]),
        ),
        text_chunk("c"),
        tool_chunk(
            json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "1}" } }]),
        ),
        tool_chunk(json!([{ "index": 1, "function": { "arguments": "}" } }])),
        text_chunk("d"),
        finish_chunk("tool_calls"),
    ];
    let events = run(&chunks, Some(USAGE));
    assert_well_formed(&events);
    assert_eq!(
        collect_response(&events).unwrap().content,
        vec![
            ContentBlock::Text { text: "a".into() },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Read".into(),
                input: json!({ "x": 1 })
            },
            ContentBlock::Text { text: "b".into() },
            ContentBlock::ToolUse {
                id: "c2".into(),
                name: "Ls".into(),
                input: json!({})
            },
            ContentBlock::Text { text: "cd".into() },
        ]
    );
}

/// A 1ª tool fecha o texto mesmo antes de poder abrir: o texto seguinte é outro
/// bloco, depois dela. Enquanto ela espera o `id`, o aberto já fechou.
#[test]
fn tool_waiting_for_its_id_closes_the_text_and_holds_what_follows() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    t.push(&text_chunk("a"));
    assert_eq!(
        names(&t.push(&tool_chunk(
            json!([{ "index": 0, "function": { "name": "Read", "arguments": "{" } }])
        ))),
        ["content_block_stop"]
    );
    assert!(t.push(&text_chunk("b")).is_empty());
    assert_eq!(
        names(&t.push(&tool_chunk(json!([{ "index": 0, "id": "c1" }])))),
        ["content_block_start", "content_block_delta"]
    );
}

#[test]
fn text_and_tool_calls_in_the_same_chunk_keep_text_first() {
    let chunk = delta_chunk(
        0,
        Delta {
            content: Some("oi".into()),
            tool_calls: serde_json::from_value(json!([
                { "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{}" } },
                { "index": 1, "id": "c2", "function": { "name": "Ls", "arguments": "{}" } },
            ]))
            .unwrap(),
            ..Delta::default()
        },
        Some("tool_calls"),
    );
    let events = run(&[chunk], None);
    assert_well_formed(&events);
    let content = collect_response(&events).unwrap().content;
    assert_eq!(content.len(), 3);
    assert_eq!(content[0], ContentBlock::Text { text: "oi".into() });
    assert!(matches!(&content[2], ContentBlock::ToolUse { id, .. } if id == "c2"));
}

/// Cliente executa tool de nome conhecido: chamada que nunca recebeu nome some, e
/// não conta para `tool_use`. A ordem do resto fica, e o `n` do `id` gerado é o da
/// ordem de aparição.
#[test]
fn tool_that_never_gets_a_name_is_dropped() {
    let events = run(
        &[
            tool_chunk(json!([{ "index": 0, "id": "c1", "function": { "arguments": "{}" } }])),
            text_chunk("fim"),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert_well_formed(&events);
    assert_eq!(stop_reason(&events), Some(StopReason::EndTurn));
    assert_eq!(
        collect_response(&events).unwrap().content,
        vec![ContentBlock::Text { text: "fim".into() }]
    );

    let events = run(
        &[
            tool_chunk(json!([{ "index": 0, "function": { "arguments": "{}" } }])),
            tool_chunk(json!([{ "index": 1, "function": { "name": "Ls", "arguments": "{}" } }])),
        ],
        None,
    );
    assert_well_formed(&events);
    assert_eq!(stop_reason(&events), Some(StopReason::ToolUse));
    assert!(matches!(
        &collect_response(&events).unwrap().content[..],
        [ContentBlock::ToolUse { id, .. }] if id == "toolu_msg_1_1"
    ));
}

/// `tool_use` sem nenhum bloco deixaria o cliente esperando tool que não existe.
#[test]
fn tool_calls_finish_without_any_tool_is_end_turn() {
    let events = run(&[text_chunk("a"), finish_chunk("tool_calls")], None);
    assert_eq!(stop_reason(&events), Some(StopReason::EndTurn));
}

/// `finish_reason: ""` é o `null` de alguns upstreams: não apaga o que já veio.
#[test]
fn empty_finish_reason_does_not_overwrite() {
    let events = run(
        &[text_chunk("a"), finish_chunk("length"), finish_chunk("")],
        None,
    );
    assert_eq!(stop_reason(&events), Some(StopReason::MaxTokens));
}

/// `usage` zerado é upstream que não contou, não resposta de custo zero.
#[test]
fn zeroed_usage_falls_back_to_the_estimates() {
    let delta_usage = |usage: Usage| {
        run(&[text_chunk("abcdefgh")], Some(usage))
            .iter()
            .find_map(|e| match e {
                StreamEvent::MessageDelta { usage, .. } => Some(*usage),
                _ => None,
            })
    };
    assert_eq!(
        delta_usage(Usage::default()),
        Some(DeltaUsage {
            input_tokens: None,
            output_tokens: 2
        })
    );
    assert_eq!(
        delta_usage(Usage {
            prompt_tokens: 20,
            ..Usage::default()
        }),
        Some(DeltaUsage {
            input_tokens: Some(20),
            output_tokens: 2
        })
    );
}

/// Pedaço depois do fim não gera nada, nem depois de `finish`.
#[test]
fn nothing_comes_out_after_finish() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    t.push(&text_chunk("a"));
    t.finish(None);
    assert!(t.push(&text_chunk("b")).is_empty());
    assert!(t.fail("api_error", "tarde").is_empty());
}

/// Falha antes de qualquer chunk: o evento `error` basta, sem `message_start`.
#[test]
fn fail_before_any_chunk_is_only_the_error() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    assert_eq!(names(&t.fail("api_error", "caiu")), ["error"]);
}

#[test]
fn encode_tool_use_id_roundtrips_and_skips_empty_signature() {
    let id = encode_tool_use_id("c1", Some("assinatura ç"));
    assert_eq!(
        decode_tool_use_id(&id),
        ("c1".to_owned(), Some("assinatura ç".to_owned()))
    );
    assert_eq!(encode_tool_use_id("c1", None), "c1");
    assert_eq!(encode_tool_use_id("c1", Some("")), "c1");
}

/// Assinatura que chega depois do `id` mas antes do nome ainda vale; o bloco só
/// abre com os dois.
#[test]
fn signature_before_the_block_opens_is_kept() {
    let events = run(
        &[
            tool_chunk(json!([{ "index": 0, "id": "c1" }])),
            tool_chunk(json!([{
                "index": 0,
                "extra_content": { "google": { "thought_signature": "S" } },
            }])),
            tool_chunk(json!([{ "index": 0, "function": { "name": "Ls", "arguments": "{}" } }])),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert!(matches!(
        &collect_response(&events).unwrap().content[..],
        [ContentBlock::ToolUse { id, .. }] if id == &encode_tool_use_id("c1", Some("S"))
    ));
}

#[test]
fn collect_rejects_an_incomplete_stream() {
    assert!(collect_response(&[]).is_err());
    let mut t = OpenAiToAnthropicStream::new(meta());
    let partial = t.push(&text_chunk("a"));
    assert!(collect_response(&partial).is_err());
}

/// Há upstream que repete `id` e `name` em todo chunk: não duplica nem troca.
#[test]
fn repeated_id_and_name_keep_the_first() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": "{" } }]),
            ),
            tool_chunk(
                json!([{ "index": 0, "id": "outro", "function": { "name": "Grep", "arguments": "}" } }]),
            ),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert_well_formed(&events);
    assert_eq!(
        collect_response(&events).unwrap().content,
        vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({})
        }]
    );
}

#[test]
fn first_non_empty_signature_wins() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "extra_content": { "google": { "thought_signature": "" } }, "function": { "name": "Ls" } }]),
            ),
            tool_chunk(
                json!([{ "index": 0, "extra_content": { "google": { "thought_signature": "A" } } }]),
            ),
            tool_chunk(
                json!([{ "index": 0, "extra_content": { "google": { "thought_signature": "B" } } }]),
            ),
            tool_chunk(json!([{ "index": 0, "id": "c1" }])),
        ],
        None,
    );
    assert!(matches!(
        &collect_response(&events).unwrap().content[..],
        [ContentBlock::ToolUse { id, .. }] if id == &encode_tool_use_id("c1", Some("A"))
    ));
}

/// O que veio junto do `finish_reason: "error"` ainda sai antes do evento `error`.
#[test]
fn error_finish_in_the_same_chunk_keeps_the_text() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    let chunk = delta_chunk(
        0,
        Delta {
            content: Some("oi".into()),
            ..Delta::default()
        },
        Some("error"),
    );
    assert_eq!(
        names(&t.push(&chunk)),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error"
        ]
    );
}

/// A estimativa conta caracteres, como o scheduler, não bytes.
#[test]
fn output_estimate_counts_chars_and_survives_bad_chars_per_token() {
    let output_tokens = |chars_per_token: f64| {
        let mut t = OpenAiToAnthropicStream::new(StreamMeta {
            chars_per_token,
            ..meta()
        });
        t.push(&text_chunk("çççç"));
        t.push(&tool_chunk(
            json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": "{\"ç\":1}" } }]),
        ));
        t.finish(None).iter().find_map(|e| match e {
            StreamEvent::MessageDelta { usage, .. } => Some(usage.output_tokens),
            _ => None,
        })
    };
    assert_eq!(output_tokens(4.0), Some(3), "11 chars / 4");
    assert_eq!(output_tokens(0.0), Some(11));
    assert_eq!(output_tokens(f64::NAN), Some(11));
}

/// Stream e não-stream iguais: `input` não objeto nem vira delta.
#[test]
fn non_object_args_emit_no_delta() {
    for args in ["null", "[1]", "3", " null"] {
        let events = run(
            &[
                tool_chunk(
                    json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": args } }]),
                ),
                tool_chunk(json!([{ "index": 0, "function": { "arguments": "{}" } }])),
                finish_chunk("tool_calls"),
            ],
            None,
        );
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ],
            "{args}"
        );
    }
}

#[test]
fn whitespace_before_the_object_is_not_sent() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Ls", "arguments": " " } }]),
            ),
            tool_chunk(json!([{ "index": 0, "function": { "arguments": " {\"a\":1}" } }])),
        ],
        None,
    );
    assert!(events.contains(&StreamEvent::ContentBlockDelta {
        index: 0,
        delta: BlockDelta::InputJsonDelta {
            partial_json: "{\"a\":1}".into()
        },
    }));
}

/// Perder os argumentos executaria a tool com `{}`.
#[test]
fn object_arguments_become_their_json_string() {
    let events = run(
        &[
            tool_chunk(
                json!([{ "index": 0, "id": "c1", "function": { "name": "Read", "arguments": { "p": "/etc" } } }]),
            ),
            finish_chunk("tool_calls"),
        ],
        None,
    );
    assert_eq!(
        collect_response(&events).unwrap().content,
        vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "Read".into(),
            input: json!({ "p": "/etc" })
        }]
    );
}

/// Delta vazio não anuncia chamada: o texto segue no mesmo bloco, ao vivo.
#[test]
fn empty_tool_delta_does_not_close_the_text() {
    let mut t = OpenAiToAnthropicStream::new(meta());
    t.push(&text_chunk("a"));
    assert!(
        t.push(&tool_chunk(
            json!([{ "index": 0, "function": { "arguments": "" } }])
        ))
        .is_empty()
    );
    assert_eq!(names(&t.push(&text_chunk("b"))), ["content_block_delta"]);
}
