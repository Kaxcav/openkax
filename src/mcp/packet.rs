//! Montagem do pacote enxuto e parser da resposta do operário (F2-3).
//!
//! `build_request` transforma um [`TaskPacket`] + arquivos do worktree numa
//! `ChatCompletionRequest` pequena; `parse_response` extrai o diff unificado
//! (ou os arquivos completos) e recusa qualquer coisa fora da lista `write`.
//! Nada aqui toca rede nem provider (ADR 0007).

use std::path::Path;

use serde_json::{Map, json};

use super::{FileMode, OutputFormat, TaskPacket, tool_error};
use crate::scheduler::estimate_tokens;
use crate::translate::openai::{ChatCompletionRequest, ChatMessage, MessageContent};

/// Versão do system prompt do operário (muda junto com o texto).
pub const SYSTEM_PROMPT_VERSION: u32 = 1;
/// Teto de caracteres do `CONVENTIONS.md` anexado.
pub const MAX_CONVENTIONS_CHARS: usize = 4000;
/// Nome do arquivo de convenções procurado na raiz do worktree.
pub const CONVENTIONS_FILE: &str = "CONVENTIONS.md";

/// System prompt fixo do operário (v1, PT-BR; ver ADR 0007).
pub const SYSTEM_PROMPT: &str = "\
Você é um operário de código. Executa UMA tarefa delimitada e devolve só o resultado.
Regras:
- Altere apenas os arquivos marcados como ESCRITA; os de LEITURA são contexto.
- Respeite os contratos (tipos, traits, assinaturas) e as restrições à risca.
- Todos os caminhos são relativos à raiz do repo, com `/`; nunca use caminho absoluto nem `..`.
- Não explique, não peça esclarecimentos: se algo for ambíguo, siga o contrato literalmente.
- Responda exatamente no formato de saída exigido ao final do pedido.";

/// Erro de montagem do pacote ou de leitura da resposta.
#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("pacote inválido: {0}")]
    Invalid(String),
    #[error("arquivo {path}: {reason}")]
    File { path: String, reason: String },
    #[error("pacote de {est} tokens excede max_packet_tokens={max} da classe")]
    TooLarge { est: u32, max: u32 },
    #[error("resposta do operário sem {0}")]
    NoOutput(&'static str),
    #[error("resposta do operário toca {path}: {reason}")]
    OutOfScope { path: String, reason: String },
    #[error("resposta do operário malformada: {0}")]
    Malformed(String),
}

impl PacketError {
    /// Código estável usado no erro estruturado das tools.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_input",
            Self::File { .. } => "file_error",
            Self::TooLarge { .. } => "packet_too_large",
            Self::NoOutput(_) | Self::Malformed(_) => "bad_worker_output",
            Self::OutOfScope { .. } => "out_of_scope",
        }
    }

    /// JSON `{error:{code, stage, message}}` para `is_error = true`.
    pub fn to_tool_error(&self) -> String {
        tool_error(self.code(), &self.to_string())
    }
}

/// Saída já validada do operário.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerOutput {
    /// Diff unificado normalizado (LF, termina em `\n`) e os paths tocados.
    Diff { diff: String, paths: Vec<String> },
    /// Arquivos completos (path relativo, conteúdo em LF).
    Files(Vec<(String, String)>),
}

/// Monta a request do operário. `root` é a raiz do worktree da tarefa.
///
/// Recusa com [`PacketError::TooLarge`] se a estimativa (a mesma do scheduler)
/// passar de `max_packet_tokens`, sem chamar provider.
pub fn build_request(
    packet: &TaskPacket,
    root: &Path,
    max_packet_tokens: u32,
    chars_per_token: f64,
) -> Result<ChatCompletionRequest, PacketError> {
    packet.validate().map_err(PacketError::Invalid)?;
    let root_canon = root.canonicalize().map_err(|e| PacketError::File {
        path: root.display().to_string(),
        reason: e.to_string(),
    })?;

    let mut body = String::new();
    body.push_str(&format!("# Objetivo\n{}\n\n", packet.objective.trim()));

    body.push_str("# Arquivos de LEITURA (contexto)\n");
    let mut any_read = false;
    for f in packet.files.iter().filter(|f| f.mode == FileMode::Read) {
        any_read = true;
        let text = read_inside(&root_canon, &f.path)?;
        body.push_str(&format!("\n## {}\n```\n{}\n```\n", f.path, text.trim_end()));
    }
    if !any_read {
        body.push_str("(nenhum)\n");
    }

    body.push_str("\n# Arquivos de ESCRITA\n");
    for f in packet.files.iter().filter(|f| f.mode == FileMode::Write) {
        if f.exists {
            let text = read_inside(&root_canon, &f.path)?;
            body.push_str(&format!(
                "\n## {} (conteúdo atual)\n```\n{}\n```\n",
                f.path,
                text.trim_end()
            ));
        } else {
            body.push_str(&format!("\n## {} (novo arquivo)\n", f.path));
        }
    }

    push_list(&mut body, "Contratos", &packet.contracts);
    push_list(&mut body, "Restrições", &packet.constraints);

    body.push_str("\n# Critério de aceite\n");
    for c in &packet.accept.verify {
        body.push_str(&format!("- `{c}`\n"));
    }
    body.push_str(&format!("Esperado: {}\n", packet.accept.expect.trim()));

    if let Some(conv) = conventions(&root_canon) {
        body.push_str(&format!("\n# Convenções do repo\n{conv}\n"));
    }

    body.push_str("\n# Formato de saída exigido\n");
    body.push_str(match packet.output {
        OutputFormat::UnifiedDiff => {
            "Devolva UM diff unificado dentro de um bloco ```diff, com cabeçalhos \
`--- a/<path>` e `+++ b/<path>` (arquivo novo: `--- /dev/null`). \
Paths relativos ao repo. Nada fora do bloco.\n"
        }
        OutputFormat::FullFile => {
            "Devolva cada arquivo COMPLETO neste formato, sem nada fora dele:\n\
=== FILE: <path> ===\n<conteúdo inteiro>\n=== END FILE ===\n"
        }
    });

    let request = ChatCompletionRequest {
        model: packet.class.clone(),
        messages: vec![
            message("system", SYSTEM_PROMPT.to_owned()),
            message("user", body),
        ],
        stream: Some(false),
        max_tokens: None,
        extra: {
            let mut m = Map::new();
            m.insert("temperature".into(), json!(0.2));
            m
        },
    };

    let est = estimate_tokens(&request, chars_per_token);
    if est > max_packet_tokens {
        return Err(PacketError::TooLarge {
            est,
            max: max_packet_tokens,
        });
    }
    Ok(request)
}

fn message(role: &str, text: String) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content: Some(MessageContent::Text(text)),
        extra: Map::new(),
    }
}

fn push_list(body: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    body.push_str(&format!("\n# {title}\n"));
    for i in items {
        body.push_str(&format!("- {}\n", i.trim()));
    }
}

/// Lê um arquivo garantindo que continua dentro do worktree (symlink incluso).
fn read_inside(root_canon: &Path, rel: &str) -> Result<String, PacketError> {
    let err = |reason: String| PacketError::File {
        path: rel.to_owned(),
        reason,
    };
    let full = root_canon
        .join(rel)
        .canonicalize()
        .map_err(|e| err(e.to_string()))?;
    if !full.starts_with(root_canon) {
        return Err(err("fora do worktree".into()));
    }
    let text = std::fs::read_to_string(&full).map_err(|e| err(e.to_string()))?;
    Ok(text.replace("\r\n", "\n"))
}

/// `CONVENTIONS.md` da raiz, truncado com aviso; `None` se não existir.
fn conventions(root_canon: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root_canon.join(CONVENTIONS_FILE)).ok()?;
    let text = text.replace("\r\n", "\n");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_CONVENTIONS_CHARS {
        return Some(text.to_owned());
    }
    let cut: String = text.chars().take(MAX_CONVENTIONS_CHARS).collect();
    Some(format!(
        "{cut}\n[... {CONVENTIONS_FILE} truncado em {MAX_CONVENTIONS_CHARS} caracteres]"
    ))
}

/// Interpreta a resposta do operário conforme o formato pedido no pacote.
pub fn parse_response(text: &str, packet: &TaskPacket) -> Result<WorkerOutput, PacketError> {
    let allowed: Vec<&str> = packet
        .files
        .iter()
        .filter(|f| f.mode == FileMode::Write)
        .map(|f| f.path.as_str())
        .collect();
    let text = text.replace("\r\n", "\n");
    match packet.output {
        OutputFormat::UnifiedDiff => parse_diff(&text, &allowed),
        OutputFormat::FullFile => parse_files(&text, &allowed),
    }
}

/// Normaliza e valida um path vindo do operário contra a lista `write`.
fn check_path(raw: &str, allowed: &[&str]) -> Result<String, PacketError> {
    let oos = |reason: &str| PacketError::OutOfScope {
        path: raw.to_owned(),
        reason: reason.to_owned(),
    };
    let p = raw.trim().replace('\\', "/");
    let bytes = p.as_bytes();
    let drive = bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    if p.is_empty() {
        return Err(oos("path vazio"));
    }
    if p.starts_with('/') || drive {
        return Err(oos("path absoluto"));
    }
    if p.split('/').any(|c| c == "..") {
        return Err(oos("path com '..'"));
    }
    let p = p.strip_prefix("./").unwrap_or(&p).to_owned();
    if !allowed.iter().any(|a| a.replace('\\', "/") == p) {
        return Err(oos("fora da lista write"));
    }
    Ok(p)
}

/// Remove `a/`/`b/` e o sufixo `\t<timestamp>` de um cabeçalho de diff.
fn header_path(rest: &str) -> &str {
    let r = rest.split('\t').next().unwrap_or(rest).trim();
    r.strip_prefix("a/")
        .or_else(|| r.strip_prefix("b/"))
        .unwrap_or(r)
}

fn parse_diff(text: &str, allowed: &[&str]) -> Result<WorkerOutput, PacketError> {
    let body = diff_body(text).ok_or(PacketError::NoOutput("diff unificado"))?;
    let mut paths: Vec<String> = Vec::new();
    let mut hunks = 0usize;
    for line in body.lines() {
        let raw = if let Some(r) = line.strip_prefix("+++ ") {
            r
        } else if let Some(r) = line.strip_prefix("--- ") {
            r
        } else if let Some(r) = line.strip_prefix("diff --git ") {
            // `diff --git a/x b/x`: confere os dois lados.
            for side in r.split_whitespace() {
                let p = check_path(header_path(side), allowed)?;
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
            continue;
        } else {
            if line.starts_with("@@") {
                hunks += 1;
            }
            continue;
        };
        let raw = raw.split('\t').next().unwrap_or(raw).trim();
        if raw == "/dev/null" {
            continue;
        }
        let p = check_path(header_path(raw), allowed)?;
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    if paths.is_empty() || hunks == 0 {
        return Err(PacketError::Malformed(
            "diff sem cabeçalhos de arquivo ou sem hunks".into(),
        ));
    }
    let mut diff = body;
    if !diff.ends_with('\n') {
        diff.push('\n');
    }
    Ok(WorkerOutput::Diff { diff, paths })
}

/// Recorta o diff: bloco cercado com `+++ `, ou do 1º cabeçalho até o fim.
fn diff_body(text: &str) -> Option<String> {
    let mut block: Option<Vec<&str>> = None;
    let mut found: Option<Vec<&str>> = None;
    for line in text.lines() {
        match (&mut block, line.trim_end()) {
            (None, l) if l.starts_with("```") => block = Some(Vec::new()),
            (Some(b), "```") => {
                if b.iter().any(|l| l.starts_with("+++ ")) {
                    found = block.take();
                    break;
                }
                block = None;
            }
            (Some(b), _) => b.push(line),
            _ => {}
        }
    }
    if let Some(b) = found {
        return Some(b.join("\n"));
    }
    // Sem cerca: do primeiro `diff --git`/`--- ` até o fim.
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with("diff --git ") || l.starts_with("--- "))?;
    let tail = &lines[start..];
    tail.iter()
        .any(|l| l.starts_with("+++ "))
        .then(|| tail.join("\n"))
}

fn parse_files(text: &str, allowed: &[&str]) -> Result<WorkerOutput, PacketError> {
    const OPEN: &str = "=== FILE: ";
    const END: &str = "=== END FILE ===";
    let mut files: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        let t = line.trim_end();
        if let Some((path, buf)) = &mut cur {
            if t == END {
                let mut content = buf.join("\n");
                content.push('\n');
                files.push((std::mem::take(path), content));
                cur = None;
            } else {
                buf.push(line);
            }
        } else if let Some(rest) = t.strip_prefix(OPEN) {
            let raw = rest.strip_suffix(" ===").unwrap_or(rest);
            cur = Some((check_path(raw, allowed)?, Vec::new()));
        }
    }
    if cur.is_some() {
        return Err(PacketError::Malformed(
            "bloco de arquivo sem `=== END FILE ===`".into(),
        ));
    }
    if files.is_empty() {
        return Err(PacketError::NoOutput("arquivos completos"));
    }
    Ok(WorkerOutput::Files(files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn packet() -> TaskPacket {
        serde_json::from_value(json!({
            "id": "T-1",
            "class": "kax/coder-smart",
            "objective": "Implementar o adapter",
            "files": [
                { "path": "src/a.rs", "mode": "read" },
                { "path": "src/b.rs", "mode": "read" },
                { "path": "src/new.rs", "mode": "write", "exists": false }
            ],
            "contracts": ["trait X { fn f(&self); }"],
            "constraints": ["sem novas dependências"],
            "accept": { "verify": ["cargo check"], "expect": "exit 0" }
        }))
        .unwrap()
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/a.rs"), "pub trait X {}\r\n").unwrap();
        fs::write(d.path().join("src/b.rs"), "pub fn b() {}\n").unwrap();
        d
    }

    fn user_text(r: &ChatCompletionRequest) -> String {
        match r.messages[1].content.as_ref().unwrap() {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(_) => panic!("esperava texto"),
        }
    }

    #[test]
    fn two_reads_and_one_write_fit() {
        let d = repo();
        let r = build_request(&packet(), d.path(), 6000, 3.6).unwrap();
        assert_eq!(r.model, "kax/coder-smart");
        assert_eq!(r.messages[0].role, "system");
        let t = user_text(&r);
        for needle in [
            "Implementar o adapter",
            "## src/a.rs",
            "pub trait X {}",
            "## src/b.rs",
            "## src/new.rs (novo arquivo)",
            "trait X { fn f(&self); }",
            "sem novas dependências",
            "`cargo check`",
            "```diff",
        ] {
            assert!(t.contains(needle), "faltou {needle:?} em:\n{t}");
        }
        assert!(!t.contains('\r'), "CRLF deve virar LF");
        assert!(estimate_tokens(&r, 3.6) <= 6000);
    }

    #[test]
    fn existing_write_file_sends_current_content() {
        let d = repo();
        fs::write(d.path().join("src/new.rs"), "fn atual() {}\n").unwrap();
        let mut p = packet();
        p.files[2].exists = true;
        let t = user_text(&build_request(&p, d.path(), 6000, 3.6).unwrap());
        assert!(t.contains("## src/new.rs (conteúdo atual)"));
        assert!(t.contains("fn atual() {}"));
    }

    #[test]
    fn full_file_format_changes_instructions() {
        let d = repo();
        let mut p = packet();
        p.output = OutputFormat::FullFile;
        let t = user_text(&build_request(&p, d.path(), 6000, 3.6).unwrap());
        assert!(t.contains("=== FILE: <path> ==="));
        assert!(!t.contains("```diff"));
    }

    #[test]
    fn big_packet_is_refused_with_structured_error() {
        let d = repo();
        fs::write(d.path().join("src/a.rs"), "x".repeat(40_000)).unwrap();
        let e = build_request(&packet(), d.path(), 6000, 3.6).unwrap_err();
        assert!(matches!(e, PacketError::TooLarge { max: 6000, .. }), "{e}");
        let v: serde_json::Value = serde_json::from_str(&e.to_tool_error()).unwrap();
        assert_eq!(v["error"]["code"], "packet_too_large");
    }

    #[test]
    fn conventions_are_attached_and_truncated() {
        let d = repo();
        fs::write(d.path().join(CONVENTIONS_FILE), "Use snake_case.\n").unwrap();
        let t = user_text(&build_request(&packet(), d.path(), 6000, 3.6).unwrap());
        assert!(t.contains("# Convenções do repo\nUse snake_case."));

        fs::write(d.path().join(CONVENTIONS_FILE), "y".repeat(9000)).unwrap();
        let t = user_text(&build_request(&packet(), d.path(), 20_000, 3.6).unwrap());
        assert!(t.contains("truncado em 4000"));
        assert!(!t.contains(&"y".repeat(4001)));
    }

    #[test]
    fn missing_read_file_is_a_file_error() {
        let d = repo();
        fs::remove_file(d.path().join("src/b.rs")).unwrap();
        let e = build_request(&packet(), d.path(), 6000, 3.6).unwrap_err();
        assert_eq!(e.code(), "file_error");
    }

    const GOOD: &str = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,2 @@\n+fn n() {}\n+\n";

    fn diff_for(path: &str) -> String {
        format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-a\n+b\n")
    }

    #[test]
    fn parser_table() {
        let p = packet();
        let fenced = format!("Aqui vai:\n```diff\n{GOOD}```\nfim");
        let crlf = fenced.replace('\n', "\r\n");
        let bare = GOOD.to_owned();
        let git = format!("diff --git a/src/new.rs b/src/new.rs\n{GOOD}");
        for (name, text) in [
            ("cercado", fenced),
            ("crlf", crlf),
            ("sem cerca", bare),
            ("diff --git", git),
        ] {
            match parse_response(&text, &p) {
                Ok(WorkerOutput::Diff { diff, paths }) => {
                    assert_eq!(paths, ["src/new.rs"], "{name}");
                    assert!(!diff.contains('\r'), "{name}");
                    assert!(diff.ends_with('\n'), "{name}");
                    assert!(diff.contains("+fn n() {}"), "{name}");
                }
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn parser_rejects_out_of_scope_and_traversal() {
        let p = packet();
        for (name, path, code) in [
            ("fora do write", "src/a.rs", "out_of_scope"),
            ("traversal", "../x.rs", "out_of_scope"),
            ("traversal interno", "src/../../x.rs", "out_of_scope"),
            ("absoluto", "/etc/passwd", "out_of_scope"),
            ("drive", "C:/x.rs", "out_of_scope"),
            ("barra invertida", "..\\x.rs", "out_of_scope"),
        ] {
            let text = format!("```diff\n{}```", diff_for(path));
            let e = parse_response(&text, &p).unwrap_err();
            assert_eq!(e.code(), code, "{name}: {e}");
        }
        // Um arquivo válido e outro fora: a resposta inteira é recusada.
        let mix = format!("{GOOD}{}", diff_for("src/b.rs"));
        assert_eq!(parse_response(&mix, &p).unwrap_err().code(), "out_of_scope");
    }

    #[test]
    fn parser_rejects_empty_or_malformed() {
        let p = packet();
        assert_eq!(
            parse_response("não sei fazer", &p).unwrap_err().code(),
            "bad_worker_output"
        );
        assert_eq!(
            parse_response("--- a/src/new.rs\n+++ b/src/new.rs\n", &p)
                .unwrap_err()
                .code(),
            "bad_worker_output"
        );
    }

    #[test]
    fn full_file_parser() {
        let mut p = packet();
        p.output = OutputFormat::FullFile;
        let ok = "=== FILE: src/new.rs ===\r\nfn n() {}\r\n=== END FILE ===\r\n";
        match parse_response(ok, &p).unwrap() {
            WorkerOutput::Files(f) => {
                assert_eq!(f, [("src/new.rs".to_owned(), "fn n() {}\n".to_owned())]);
            }
            other => panic!("{other:?}"),
        }
        let bad = "=== FILE: src/a.rs ===\nx\n=== END FILE ===";
        assert_eq!(parse_response(bad, &p).unwrap_err().code(), "out_of_scope");
        let trav = "=== FILE: ../x ===\nx\n=== END FILE ===";
        assert_eq!(parse_response(trav, &p).unwrap_err().code(), "out_of_scope");
        let open = "=== FILE: src/new.rs ===\nx\n";
        assert_eq!(
            parse_response(open, &p).unwrap_err().code(),
            "bad_worker_output"
        );
    }
}
