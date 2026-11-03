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
    let write = write_paths(packet);
    let allowed: Vec<&str> = write.iter().map(String::as_str).collect();
    let text = text.replace("\r\n", "\n");
    match packet.output {
        OutputFormat::UnifiedDiff => parse_diff(&text, &allowed),
        OutputFormat::FullFile => parse_files(&text, &allowed),
    }
}

/// Paths `write` do pacote, normalizados como o parser e o git os escrevem.
pub fn write_paths(packet: &TaskPacket) -> Vec<String> {
    packet
        .files
        .iter()
        .filter(|f| f.mode == FileMode::Write)
        .map(|f| normalize_path(&f.path))
        .collect()
}

/// `\` vira `/` e some o `./` inicial.
fn normalize_path(raw: &str) -> String {
    let p = raw.trim().replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_owned()
}

/// Normaliza e valida um path vindo do operário contra a lista `write`.
fn check_path(raw: &str, allowed: &[&str]) -> Result<String, PacketError> {
    let oos = |reason: &str| PacketError::OutOfScope {
        path: raw.to_owned(),
        reason: reason.to_owned(),
    };
    let p = normalize_path(raw);
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
    if !allowed.contains(&p.as_str()) {
        return Err(oos("fora da lista write"));
    }
    Ok(p)
}

fn malformed(msg: impl Into<String>) -> PacketError {
    PacketError::Malformed(msg.into())
}

/// Modos de arquivo comum, os únicos aceitos em `new file mode`,
/// `deleted file mode` e na linha `index`.
const REGULAR_MODES: [&str; 2] = ["100644", "100755"];

/// Cabeçalhos estendidos do git que o operário nunca pode usar: renomeiam,
/// copiam, mudam modo ou trazem binário fora da lista `write`.
const FORBIDDEN_HEADERS: [(&str, &str); 8] = [
    ("rename ", "rename não é permitido"),
    ("copy ", "copy não é permitido"),
    ("similarity index ", "rename/copy não é permitido"),
    ("dissimilarity index ", "rename/copy não é permitido"),
    ("old mode ", "mudança de modo não é permitida"),
    ("new mode ", "mudança de modo não é permitida"),
    ("GIT binary patch", "patch binário não é permitido"),
    ("Binary files ", "patch binário não é permitido"),
];

/// Onde o parser está. Segue a gramática do `git apply`: cabeçalho de arquivo
/// e hunks cujo tamanho vem do `@@`. Linha dentro de hunk é conteúdo e nunca
/// é lida como cabeçalho; fora de hunk, o que o git interpretaria é conferido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffState {
    /// Antes do 1º arquivo ou depois de um hunk. Texto solto aqui o git ignora.
    Between,
    /// Depois de `diff --git`, antes de `---`/`@@`.
    GitHeader,
    /// Depois de `---`: a próxima linha tem de ser `+++`.
    Minus,
    /// Depois de `+++`: a próxima linha tem de ser `@@`.
    Plus,
    /// Linhas que ainda faltam do lado antigo e do novo.
    Hunk { old: u64, new: u64 },
}

struct DiffParser<'a> {
    allowed: &'a [&'a str],
    state: DiffState,
    /// Path do `diff --git` do arquivo atual (`None` em diff tradicional).
    git_path: Option<String>,
    /// Path do `---` atual (`None` = `/dev/null`).
    minus: Option<String>,
    /// O arquivo atual já teve hunk: outro `@@` em seguida é do mesmo arquivo.
    in_file: bool,
    hunks: usize,
    paths: Vec<String>,
}

impl DiffParser<'_> {
    fn line(&mut self, line: &str) -> Result<(), PacketError> {
        self.state = match self.state {
            DiffState::Hunk { old, new } => hunk_line(line, old, new)?,
            DiffState::Between => self.between(line)?,
            DiffState::GitHeader => self.git_header(line)?,
            DiffState::Minus => {
                let rest = line
                    .strip_prefix("+++ ")
                    .ok_or_else(|| malformed("`---` sem `+++` na linha seguinte"))?;
                self.plus(rest)?;
                DiffState::Plus
            }
            DiffState::Plus if line.starts_with("@@") => self.hunk(line)?,
            DiffState::Plus => return Err(malformed("`+++` sem hunk `@@` na linha seguinte")),
        };
        Ok(())
    }

    fn between(&mut self, line: &str) -> Result<DiffState, PacketError> {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            return self.git_start(rest);
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            self.git_path = None;
            self.in_file = false;
            self.minus(rest)?;
            return Ok(DiffState::Minus);
        }
        if line.starts_with("+++ ") {
            return Err(malformed("`+++` sem `---` antes"));
        }
        if line.starts_with("@@") {
            if !self.in_file {
                return Err(malformed("hunk `@@` sem cabeçalho de arquivo"));
            }
            return self.hunk(line);
        }
        if line.starts_with(['+', '-', ' ']) {
            return Err(malformed(format!(
                "linha de hunk fora de hunk (a contagem do `@@` não bate?): {line:?}"
            )));
        }
        // Linha em branco, `\ No newline at end of file` ou texto solto.
        Ok(DiffState::Between)
    }

    /// `diff --git a/<p> b/<p>`: o mesmo path dos dois lados (sem rename).
    fn git_start(&mut self, rest: &str) -> Result<DiffState, PacketError> {
        let rest = rest.trim_end();
        let half = rest.len().saturating_sub(5) / 2;
        let same = rest
            .get(2..2 + half)
            .filter(|p| rest == format!("a/{p} b/{p}"));
        let Some(p) = same else {
            return Err(PacketError::OutOfScope {
                path: rest.to_owned(),
                reason: "`diff --git` precisa de `a/<path> b/<path>` com o mesmo path".into(),
            });
        };
        let p = check_path(p, self.allowed)?;
        self.add_path(&p);
        self.git_path = Some(p);
        self.in_file = false;
        Ok(DiffState::GitHeader)
    }

    /// Entre `diff --git` e o primeiro `---`/`@@`: só `index`, `new file mode`
    /// e `deleted file mode` de arquivo comum.
    fn git_header(&mut self, line: &str) -> Result<DiffState, PacketError> {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Arquivo anterior só com cabeçalho (ex.: arquivo novo vazio).
            return self.git_start(rest);
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            self.minus(rest)?;
            return Ok(DiffState::Minus);
        }
        if line.starts_with("@@") {
            return self.hunk(line);
        }
        let path = self.git_path.clone().unwrap_or_default();
        let oos = |reason: String| PacketError::OutOfScope {
            path: path.clone(),
            reason,
        };
        if let Some(rest) = line.strip_prefix("index ") {
            let (range, mode) = rest.split_once(' ').unwrap_or((rest, ""));
            let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
            if !range
                .split_once("..")
                .is_some_and(|(a, b)| hex(a) && hex(b))
            {
                return Err(malformed(format!("linha `index` inválida: {line:?}")));
            }
            if !mode.is_empty() && !REGULAR_MODES.contains(&mode) {
                return Err(oos(format!("modo {mode} não permitido na linha `index`")));
            }
            return Ok(DiffState::GitHeader);
        }
        for prefix in ["new file mode ", "deleted file mode "] {
            if let Some(mode) = line.strip_prefix(prefix) {
                if REGULAR_MODES.contains(&mode) {
                    return Ok(DiffState::GitHeader);
                }
                return Err(oos(format!(
                    "modo {mode} não permitido (symlink, submódulo...): só 100644 ou 100755"
                )));
            }
        }
        if let Some((_, why)) = FORBIDDEN_HEADERS.iter().find(|(h, _)| line.starts_with(h)) {
            return Err(oos(format!("{why}: {line:?}")));
        }
        Err(malformed(format!(
            "linha inesperada no cabeçalho de {path}: {line:?}"
        )))
    }

    fn minus(&mut self, rest: &str) -> Result<(), PacketError> {
        let m = side_path(rest, self.allowed)?;
        self.same_as_git(m.as_deref())?;
        self.minus = m;
        Ok(())
    }

    fn plus(&mut self, rest: &str) -> Result<(), PacketError> {
        let p = side_path(rest, self.allowed)?;
        self.same_as_git(p.as_deref())?;
        match (&self.minus, &p) {
            (None, None) => return Err(malformed("`---` e `+++` ambos em /dev/null")),
            (Some(m), Some(q)) if m != q => {
                return Err(malformed(format!("`--- {m}` e `+++ {q}` não batem")));
            }
            _ => {}
        }
        for path in [self.minus.clone(), p].into_iter().flatten() {
            self.add_path(&path);
        }
        Ok(())
    }

    /// `---`/`+++` de um `diff --git` têm de nomear o mesmo arquivo dele.
    fn same_as_git(&self, side: Option<&str>) -> Result<(), PacketError> {
        match (&self.git_path, side) {
            (Some(g), Some(s)) if g != s => {
                Err(malformed(format!("`{s}` não bate com `diff --git` de {g}")))
            }
            _ => Ok(()),
        }
    }

    fn hunk(&mut self, line: &str) -> Result<DiffState, PacketError> {
        let (old, new) = hunk_counts(line)
            .ok_or_else(|| malformed(format!("cabeçalho de hunk inválido: {line:?}")))?;
        if old == 0 && new == 0 {
            return Err(malformed(format!("hunk vazio: {line:?}")));
        }
        self.hunks += 1;
        self.in_file = true;
        Ok(DiffState::Hunk { old, new })
    }

    fn add_path(&mut self, p: &str) {
        if !self.paths.iter().any(|x| x == p) {
            self.paths.push(p.to_owned());
        }
    }
}

/// Path de `---`/`+++` (`None` = `/dev/null`). Exige o prefixo `a/`/`b/`: o
/// `git apply` corta o 1º componente (`-p1`) e sem ele escreveria noutro lugar.
fn side_path(rest: &str, allowed: &[&str]) -> Result<Option<String>, PacketError> {
    let raw = rest.split('\t').next().unwrap_or(rest).trim();
    if raw == "/dev/null" {
        return Ok(None);
    }
    let p = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .ok_or_else(|| PacketError::OutOfScope {
            path: raw.to_owned(),
            reason: "path sem prefixo a/ ou b/".into(),
        })?;
    check_path(p, allowed).map(Some)
}

/// `@@ -a[,b] +c[,d] @@`: devolve `(b, d)`; contagem omitida vale 1, como no git.
fn hunk_counts(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    Some((range_len(old)?, range_len(new)?))
}

fn range_len(range: &str) -> Option<u64> {
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    match range.split_once(',') {
        Some((start, len)) => num(start).and(num(len)),
        None => num(range).map(|_| 1),
    }
}

/// Consome uma linha de hunk e diz onde o parser fica.
fn hunk_line(line: &str, old: u64, new: u64) -> Result<DiffState, PacketError> {
    let short = || malformed("hunk com mais linhas do que o `@@` declara");
    let (old, new) = match line.as_bytes().first() {
        // Linha vazia é contexto vazio (o git aceita igual).
        None | Some(b' ') => (
            old.checked_sub(1).ok_or_else(short)?,
            new.checked_sub(1).ok_or_else(short)?,
        ),
        Some(b'-') => (old.checked_sub(1).ok_or_else(short)?, new),
        Some(b'+') => (old, new.checked_sub(1).ok_or_else(short)?),
        Some(b'\\') => (old, new),
        Some(_) => {
            return Err(malformed(format!(
                "linha inválida dentro de hunk: {line:?}"
            )));
        }
    };
    Ok(if old == 0 && new == 0 {
        DiffState::Between
    } else {
        DiffState::Hunk { old, new }
    })
}

fn parse_diff(text: &str, allowed: &[&str]) -> Result<WorkerOutput, PacketError> {
    let body = diff_body(text).ok_or(PacketError::NoOutput("diff unificado"))?;
    let mut parser = DiffParser {
        allowed,
        state: DiffState::Between,
        git_path: None,
        minus: None,
        in_file: false,
        hunks: 0,
        paths: Vec::new(),
    };
    for line in body.lines() {
        parser.line(line)?;
    }
    match parser.state {
        DiffState::Hunk { .. } => {
            return Err(malformed(
                "diff acaba no meio de um hunk (o `@@` declara mais linhas)",
            ));
        }
        DiffState::Minus | DiffState::Plus => {
            return Err(malformed("diff acaba num cabeçalho `---`/`+++` sem hunk"));
        }
        DiffState::Between | DiffState::GitHeader => {}
    }
    if parser.paths.is_empty() || parser.hunks == 0 {
        return Err(malformed("diff sem cabeçalhos de arquivo ou sem hunks"));
    }
    let mut diff = body;
    if !diff.ends_with('\n') {
        diff.push('\n');
    }
    Ok(WorkerOutput::Diff {
        diff,
        paths: parser.paths,
    })
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
            MessageContent::Null => panic!("esperava texto"),
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

    /// Gerados com o código de antes dos campos `symbols`/`lines` (P6a).
    const GOLDEN_REQUEST: &str = include_str!("../../tests/fixtures/mcp/pacote_sem_seletores.json");
    const GOLDEN_PACKET: &str =
        include_str!("../../tests/fixtures/mcp/task_packet_sem_seletores.json");

    #[test]
    fn packet_without_selectors_is_unchanged() {
        let d = repo();
        let r = build_request(&packet(), d.path(), 6000, 3.6).unwrap();
        let got = serde_json::to_string_pretty(&r).unwrap() + "\n";
        assert_eq!(got, GOLDEN_REQUEST.replace("\r\n", "\n"));

        let got = serde_json::to_string_pretty(&packet()).unwrap() + "\n";
        assert_eq!(got, GOLDEN_PACKET.replace("\r\n", "\n"));
        let back: TaskPacket = serde_json::from_str(GOLDEN_PACKET).unwrap();
        assert_eq!(back, packet());
    }

    #[test]
    fn selectors_are_refused_not_ignored() {
        let d = repo();
        let mut p = packet();
        p.files[0].symbols = Some(vec!["X".into()]);
        let e = build_request(&p, d.path(), 6000, 3.6).unwrap_err();
        assert!(
            matches!(e, PacketError::Invalid(ref m) if m.contains("P6d")),
            "{e}"
        );
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

    /// Pacote com `src/a.rs` e `src/new.rs` na lista `write`.
    fn two_writes() -> TaskPacket {
        let mut p = packet();
        p.files[0].mode = FileMode::Write;
        p
    }

    const HUNK_A: &str =
        "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n";

    fn refused(text: &str, p: &TaskPacket) -> PacketError {
        let e = parse_response(text, p).expect_err("o parser devia recusar");
        assert!(
            matches!(
                e,
                PacketError::OutOfScope { .. } | PacketError::Malformed(_)
            ),
            "{e}"
        );
        e
    }

    /// C2 (a): bloco extra com rename para fora da lista `write`.
    #[test]
    fn parser_rejects_rename_block() {
        let text = format!(
            "{HUNK_A}diff --git a/src/a.rs b/src/a.rs\nrename from src/keep.rs\nrename to src/evil.rs\n"
        );
        let e = refused(&text, &two_writes());
        assert!(e.to_string().contains("rename"), "{e}");
    }

    /// C2 (b): copy disfarçado antes de um arquivo novo legítimo.
    #[test]
    fn parser_rejects_copy_block() {
        let text = format!(
            "diff --git a/src/new.rs b/src/new.rs\nsimilarity index 100%\ncopy from src/keep.rs\ncopy to src/evil2.rs\ndiff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\n{GOOD}"
        );
        let e = refused(&text, &packet());
        assert!(e.to_string().contains("copy"), "{e}");
    }

    /// C2 (c): arquivo novo como symlink (e o mesmo truque pela linha `index`).
    #[test]
    fn parser_rejects_symlink_and_odd_modes() {
        let link = "diff --git a/src/new.rs b/src/new.rs\nnew file mode 120000\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+../../../../../etc/passwd\n\\ No newline at end of file\n";
        assert!(refused(link, &packet()).to_string().contains("120000"));
        let hunk = &HUNK_A[HUNK_A.find("---").unwrap()..];
        let via_index =
            format!("diff --git a/src/a.rs b/src/a.rs\nindex 1111111..2222222 120000\n{hunk}");
        assert!(
            refused(&via_index, &two_writes())
                .to_string()
                .contains("120000")
        );
        for header in [
            "new file mode 160000",
            "deleted file mode 120000",
            "old mode 100644",
            "new mode 100755",
            "dissimilarity index 90%",
            "GIT binary patch",
            "Binary files a/src/a.rs and b/src/a.rs differ",
        ] {
            // O hunk depois do cabeçalho faz o texto ser reconhecido como diff.
            let text = format!("diff --git a/src/a.rs b/src/a.rs\n{header}\n{hunk}");
            let e = refused(&text, &two_writes());
            assert_eq!(e.code(), "out_of_scope", "{header}: {e}");
        }
    }

    #[test]
    fn parser_accepts_regular_headers() {
        let text = format!(
            "diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\nindex 0000000..1234abc\n{GOOD}diff --git a/src/a.rs b/src/a.rs\nindex 1111111..2222222 100644\n{}",
            &HUNK_A[HUNK_A.find("---").unwrap()..]
        );
        match parse_response(&text, &two_writes()).unwrap() {
            WorkerOutput::Diff { paths, .. } => assert_eq!(paths, ["src/new.rs", "src/a.rs"]),
            other => panic!("{other:?}"),
        }
        let del = "diff --git a/src/a.rs b/src/a.rs\ndeleted file mode 100644\n--- a/src/a.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n";
        assert!(parse_response(del, &two_writes()).is_ok());
    }

    /// Linhas `--- `/`+++ ` dentro de hunk são conteúdo (ex.: comentário SQL/Lua
    /// removido), não cabeçalho: antes viravam falso `out_of_scope`.
    #[test]
    fn hunk_lines_that_look_like_headers_are_content() {
        let text = "--- a/src/new.rs\n+++ b/src/new.rs\n@@ -1,3 +1,3 @@\n--- comentário antigo\n+++ comentário novo\n fn n() {}\n--- a/src/fora.rs\n++++ b/src/fora.rs\n";
        match parse_response(text, &packet()) {
            Ok(WorkerOutput::Diff { paths, .. }) => assert_eq!(paths, ["src/new.rs"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parser_requires_consistent_prefixed_paths() {
        let p = two_writes();
        // Sem `a/`/`b/`, o `git apply -p1` cortaria `src/` e gravaria `new.rs` na raiz.
        let bare = "--- /dev/null\n+++ src/new.rs\n@@ -0,0 +1 @@\n+x\n";
        assert_eq!(refused(bare, &p).code(), "out_of_scope");
        let renamed = "diff --git a/src/a.rs b/src/new.rs\n--- a/src/a.rs\n+++ b/src/new.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(refused(renamed, &p).code(), "out_of_scope");
        let mismatch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/new.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(refused(mismatch, &p).code(), "bad_worker_output");
        let short = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-a\n+b\n";
        assert_eq!(refused(short, &p).code(), "bad_worker_output");
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
