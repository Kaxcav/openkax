//! Resolução heurística de referências (ADR 0009, decisão 5).
//!
//! Algoritmo:
//! 1. **Candidatos**: definições com `name == ref.name` e espécie compatível:
//!    `call` → fn, struct, class (construtor); `call` com `method` → method;
//!    `call` com `qual_hint` e sem `method` também aceita method (`Rect::new`,
//!    `Rect.square`); `type` → struct, enum, trait, type, class, interface;
//!    `use` → qualquer.
//! 2. **Hint**: com `qual_hint`, normaliza `Self`, `this` (TS), `self`/`cls`
//!    (Python) e o `self` de `self.f()` em Rust (tipo do contêiner do sítio),
//!    `crate`/`self::`/`super` (módulo do arquivo, mais os `mod` inline) e
//!    expande um import no 1º segmento (`Store::new` com `use
//!    crate::store::Store`); o caminho dos `use`/`import` relativos (`./m`,
//!    `.m`) sai da pasta do arquivo. Hint absoluto (`crate::`, `Self`,
//!    relativo) tem de ser igual ao contêiner do candidato (`module` mais os
//!    contêineres do `qual_name`; `crate::` também exige o mesmo crate, a
//!    pasta antes do último `src/`); os outros, sufixo dele. Sem `qual_hint`,
//!    um import explícito do nome no arquivo (o alias aponta o nome original)
//!    filtra pelo caminho do import. Filtro que zera →
//!    [`Resolution::Unresolved`] (nome externo).
//! 3. **Proximidade** ([`Tier`]): mesmo arquivo > mesmo módulo (mesmo
//!    diretório) > importado (import explícito, glob ou de módulo) > global.
//!    O degrau de um candidato é o mais perto que se aplica a ele: um item
//!    importado de um arquivo vizinho fica em `SameModule`.
//! 4. No melhor degrau com candidatos: 1 → [`Resolution::Resolved`];
//!    vários → [`Resolution::Ambiguous`]. Nunca escolhe entre empatados.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    ContextError, FileFacts, Import, Ref, RefKind, Symbol, SymbolId, SymbolKind, adapter_for,
};

/// Degrau de proximidade em que a definição foi achada (do mais perto ao mais
/// longe; a ordem do `enum` é a da preferência).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    SameFile,
    /// Mesmo diretório: pacote Python, pasta TS, módulos irmãos em Rust.
    SameModule,
    /// Por import explícito, glob ou de módulo do arquivo do sítio.
    Imported,
    Global,
}

/// Resultado da resolução de um sítio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Resolution {
    Resolved {
        target: SymbolId,
        tier: Tier,
    },
    /// Empate no melhor degrau: todos os candidatos, em ordem de id.
    Ambiguous {
        candidates: Vec<SymbolId>,
        tier: Tier,
    },
    /// Nenhuma definição no índice (std, crate externa, `node_modules`).
    Unresolved,
}

impl Resolution {
    /// `true` quando `id` é o alvo ou um dos candidatos empatados.
    pub fn mentions(&self, id: &SymbolId) -> bool {
        match self {
            Self::Resolved { target, .. } => target == id,
            Self::Ambiguous { candidates, .. } => candidates.contains(id),
            Self::Unresolved => false,
        }
    }
}

/// Sintaxe de caminho da linguagem de um arquivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    Rust,
    Ts,
    Py,
    Other,
}

impl Syntax {
    pub(crate) fn of(file: &str) -> Self {
        match adapter_for(Path::new(file)).map(|a| a.id()) {
            Some("rust") => Self::Rust,
            Some("typescript" | "javascript") => Self::Ts,
            Some("python") => Self::Py,
            _ => Self::Other,
        }
    }

    /// Separador do `qual_name`.
    pub(crate) fn sep(self) -> &'static str {
        match self {
            Self::Rust => "::",
            _ => ".",
        }
    }
}

/// Tabela global de definições e imports, atualizada arquivo a arquivo (um
/// arquivo alterado sai e entra de novo, sem reconstruir o resto).
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    files: BTreeMap<String, FileFacts>,
    /// Nome curto → ids, em ordem de id.
    by_name: HashMap<String, Vec<SymbolId>>,
    /// Id → arquivo e posição em `FileFacts::symbols`.
    by_id: HashMap<SymbolId, (String, usize)>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arquivos presentes na tabela.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Símbolos de todos os arquivos.
    pub fn symbol_count(&self) -> usize {
        self.by_id.len()
    }

    /// Insere (ou troca) os fatos de `file`.
    pub fn insert_file(&mut self, file: &str, facts: FileFacts) -> Result<(), ContextError> {
        self.remove_file(file)?;
        for (i, s) in facts.symbols.iter().enumerate() {
            self.by_id.insert(s.id.clone(), (file.to_string(), i));
            let ids = self.by_name.entry(s.name.clone()).or_default();
            if let Err(at) = ids.binary_search(&s.id) {
                ids.insert(at, s.id.clone());
            }
        }
        self.files.insert(file.to_string(), facts);
        Ok(())
    }

    /// Tira `file` e tudo o que ele definia.
    pub fn remove_file(&mut self, file: &str) -> Result<(), ContextError> {
        let Some(facts) = self.files.remove(file) else {
            return Ok(());
        };
        for s in facts.symbols {
            self.by_id.remove(&s.id);
            if let Some(ids) = self.by_name.get_mut(&s.name) {
                ids.retain(|id| *id != s.id);
                if ids.is_empty() {
                    self.by_name.remove(&s.name);
                }
            }
        }
        Ok(())
    }

    /// Fatos de `file`.
    pub fn file(&self, file: &str) -> Option<&FileFacts> {
        self.files.get(file)
    }

    /// Todos os arquivos, em ordem de caminho.
    pub fn files(&self) -> impl Iterator<Item = (&str, &FileFacts)> {
        self.files.iter().map(|(f, facts)| (f.as_str(), facts))
    }

    /// Definição de `id`.
    pub fn symbol(&self, id: &SymbolId) -> Option<&Symbol> {
        let (file, i) = self.by_id.get(id)?;
        self.files.get(file)?.symbols.get(*i)
    }

    /// Definições que casam com a query (nome, caminho qualificado por sufixo
    /// ou id), em ordem de id.
    ///
    /// É id quando o 1º segmento antes de `::` termina numa extensão com
    /// adapter (`src/store.rs::validate`). O nome inteiro também vale, para o
    /// `impl fmt::Display for X` ser achado pelo cabeçalho.
    pub fn lookup(&self, query: &str) -> Result<Vec<SymbolId>, ContextError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if let Some((first, _)) = q.split_once("::")
            && adapter_for(Path::new(first)).is_some()
        {
            let id = SymbolId(q.to_string());
            return Ok(if self.by_id.contains_key(&id) {
                vec![id]
            } else {
                Vec::new()
            });
        }
        let mut out = self.by_name.get(q).cloned().unwrap_or_default();
        let segs: Vec<&str> = q
            .split("::")
            .flat_map(|p| p.split('.'))
            .filter(|s| !s.is_empty())
            .collect();
        if segs.len() > 1
            && let Some(ids) = segs.last().and_then(|n| self.by_name.get(*n))
        {
            for id in ids {
                if let Some(s) = self.symbol(id)
                    && ends_with(&self.full_path(s), &segs)
                {
                    out.push(id.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Módulo do arquivo mais os contêineres do `qual_name`, sem o nome:
    /// `["geometry", "Rect"]` para `Rect::new` em `src/geometry.rs`.
    pub(crate) fn container(&self, s: &Symbol) -> Vec<String> {
        let mut v = self
            .files
            .get(&s.file)
            .map(|f| f.module.clone())
            .unwrap_or_default();
        let sep = Syntax::of(&s.file).sep();
        let prefix = match s.qual_name.strip_suffix(s.name.as_str()) {
            Some("") => Some(""),
            Some(p) => p.strip_suffix(sep),
            None => None,
        };
        match prefix {
            Some(p) => v.extend(p.split(sep).filter(|x| !x.is_empty()).map(String::from)),
            None => {
                let mut parts: Vec<&str> = s.qual_name.split(sep).collect();
                parts.pop();
                v.extend(parts.into_iter().map(String::from));
            }
        }
        v
    }

    /// [`Self::container`] mais o nome.
    pub(crate) fn full_path(&self, s: &Symbol) -> Vec<String> {
        let mut v = self.container(s);
        v.push(s.name.clone());
        v
    }

    /// Liga um sítio a uma definição (algoritmo no topo do módulo).
    pub fn resolve(&self, r: &Ref) -> Result<Resolution, ContextError> {
        Ok(self.resolve_ref(r))
    }

    /// Sítios cuja resolução menciona um de `defs`, em ordem de arquivo e
    /// byte. Com `only_calls`, só `call`.
    pub fn sites(&self, defs: &[SymbolId], only_calls: bool) -> Vec<(&Ref, Resolution)> {
        let names: HashSet<&str> = defs
            .iter()
            .filter_map(|d| self.symbol(d))
            .map(|s| s.name.as_str())
            .collect();
        let mut out = Vec::new();
        for facts in self.files.values() {
            let alias: HashMap<&str, &str> = facts
                .imports
                .iter()
                .filter_map(|i| Some((i.alias.as_deref()?, i.name.as_deref()?)))
                .collect();
            for r in &facts.refs {
                if only_calls && r.kind != RefKind::Call {
                    continue;
                }
                let name = alias.get(r.name.as_str()).copied().unwrap_or(&r.name);
                if !names.contains(name) {
                    continue;
                }
                let res = self.resolve_ref(r);
                if defs.iter().any(|d| res.mentions(d)) {
                    out.push((r, res));
                }
            }
        }
        out.sort_by(|a, b| (&a.0.file, a.0.span.start_byte).cmp(&(&b.0.file, b.0.span.start_byte)));
        out
    }

    pub(crate) fn resolve_ref(&self, r: &Ref) -> Resolution {
        let syn = Syntax::of(&r.file);
        let imports: &[Import] = self.files.get(&r.file).map_or(&[], |f| &f.imports);
        let mut name = r.name.as_str();
        let filter = match &r.qual_hint {
            Some(hint) => Some(self.hint_filter(hint, r, syn, imports)),
            None => match local_import(imports, &r.name).filter(|i| i.name.is_some()) {
                Some(imp) => {
                    name = imp.name.as_deref().unwrap_or(name);
                    Some(self.source_filter(&imp.source, r, syn, imports))
                }
                None => None,
            },
        };
        let Some(ids) = self.by_name.get(name) else {
            return Resolution::Unresolved;
        };
        let reach: Vec<PathFilter> = imports
            .iter()
            .map(|i| self.import_target(i, r, syn, imports))
            .collect();
        let mut best: Option<Tier> = None;
        let mut picked: Vec<SymbolId> = Vec::new();
        for id in ids {
            let Some(s) = self.symbol(id) else {
                continue;
            };
            if !kind_fits(r, s.kind) {
                continue;
            }
            let container = self.container(s);
            if let Some(f) = &filter
                && !f.matches(&container, &s.file)
            {
                continue;
            }
            let tier = if s.file == r.file {
                Tier::SameFile
            } else if dir_of(&s.file) == dir_of(&r.file) {
                Tier::SameModule
            } else {
                let mut full = container;
                full.push(s.name.clone());
                if reach.iter().any(|t| t.covers(&full, &s.file)) {
                    Tier::Imported
                } else {
                    Tier::Global
                }
            };
            match best {
                Some(b) if tier > b => {}
                Some(b) if tier == b => picked.push(id.clone()),
                _ => {
                    best = Some(tier);
                    picked = vec![id.clone()];
                }
            }
        }
        match best {
            None => Resolution::Unresolved,
            Some(tier) if picked.len() == 1 => Resolution::Resolved {
                target: picked.remove(0),
                tier,
            },
            Some(tier) => Resolution::Ambiguous {
                candidates: picked,
                tier,
            },
        }
    }

    /// Filtro do `qual_hint` de um sítio.
    fn hint_filter(&self, hint: &str, r: &Ref, syn: Syntax, imports: &[Import]) -> PathFilter {
        match syn {
            Syntax::Rust => {
                let segs = split_nonempty(hint, "::");
                // O `use` não expande imports: seu hint é a própria origem.
                self.rust_path(&segs, r, imports, r.kind != RefKind::Use)
            }
            Syntax::Ts | Syntax::Py | Syntax::Other => {
                if r.kind == RefKind::Use || is_relative_source(hint, syn) {
                    return module_path(hint, &r.file, syn);
                }
                let segs = split_nonempty(hint, ".");
                let Some(&first) = segs.first() else {
                    return PathFilter::loose(&segs);
                };
                let rest = segs[1..].iter().map(|s| (*s).to_string());
                let is_self = match syn {
                    Syntax::Ts => first == "this",
                    Syntax::Py => first == "self" || first == "cls",
                    _ => false,
                };
                if is_self {
                    return match self.container_type(r) {
                        Some(mut t) => {
                            t.extend(rest);
                            PathFilter::anchored(t, None)
                        }
                        None => PathFilter::loose(&segs),
                    };
                }
                if let Some(imp) = local_import(imports, first) {
                    let mut f = self.import_target(imp, r, syn, imports);
                    f.segs.extend(rest);
                    return f;
                }
                PathFilter::loose(&segs)
            }
        }
    }

    /// Onde fica o que vem de `source` (a origem de um import).
    fn source_filter(&self, source: &str, r: &Ref, syn: Syntax, imports: &[Import]) -> PathFilter {
        match syn {
            Syntax::Rust => self.rust_path(&split_nonempty(source, "::"), r, imports, false),
            _ => module_path(source, &r.file, syn),
        }
    }

    /// Caminho completo do que um import traz (origem + nome).
    fn import_target(&self, imp: &Import, r: &Ref, syn: Syntax, imports: &[Import]) -> PathFilter {
        let mut f = self.source_filter(&imp.source, r, syn, imports);
        if let Some(n) = &imp.name {
            f.segs.push(n.clone());
        }
        f
    }

    /// Caminho Rust (`crate::a`, `self::a`, `super::a`, `Self`, `Tipo`).
    fn rust_path(&self, segs: &[&str], r: &Ref, imports: &[Import], expand: bool) -> PathFilter {
        let Some(&first) = segs.first() else {
            return PathFilter::loose(segs);
        };
        let rest = || segs[1..].iter().map(|s| (*s).to_string());
        let krate = || Some(crate_root(&r.file));
        match first {
            "crate" => PathFilter::anchored(rest().collect(), krate()),
            "Self" => self.self_type_path(r, segs),
            "self" if r.method => self.self_type_path(r, segs),
            "self" => {
                let mut base = self.scope_module(r);
                base.extend(rest());
                PathFilter::anchored(base, krate())
            }
            "super" => {
                let mut base = self.scope_module(r);
                let n = segs.iter().take_while(|s| **s == "super").count();
                for _ in 0..n {
                    base.pop();
                }
                base.extend(segs[n..].iter().map(|s| (*s).to_string()));
                PathFilter::anchored(base, krate())
            }
            _ => {
                if expand && let Some(imp) = local_import(imports, first) {
                    let mut src = split_nonempty(&imp.source, "::");
                    if let Some(n) = &imp.name {
                        src.push(n);
                    }
                    let mut f = self.rust_path(&src, r, imports, false);
                    f.segs.extend(rest());
                    return f;
                }
                PathFilter::loose(segs)
            }
        }
    }

    /// `Self::x` (e `self.x()` em Rust): tipo do contêiner + o resto.
    fn self_type_path(&self, r: &Ref, segs: &[&str]) -> PathFilter {
        match self.container_type(r) {
            Some(mut t) => {
                t.extend(segs[1..].iter().map(|s| (*s).to_string()));
                PathFilter::anchored(t, None)
            }
            None => PathFilter::loose(segs),
        }
    }

    /// Caminho do tipo em volta do sítio: o dono do método, o próprio tipo,
    /// o tipo do `impl`.
    fn container_type(&self, r: &Ref) -> Option<Vec<String>> {
        let mut cur = r.enclosing.as_ref().and_then(|id| self.symbol(id));
        while let Some(s) = cur {
            match s.kind {
                SymbolKind::Method => return Some(self.container(s)),
                SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Trait
                | SymbolKind::Class
                | SymbolKind::Interface => return Some(self.full_path(s)),
                SymbolKind::Impl => {
                    let mut v = self.container(s);
                    v.push(impl_self_type(&s.name));
                    return Some(v);
                }
                _ => cur = s.parent.as_ref().and_then(|p| self.symbol(p)),
            }
        }
        None
    }

    /// Módulo do arquivo mais os `mod` inline em volta do sítio.
    fn scope_module(&self, r: &Ref) -> Vec<String> {
        let mut v = self
            .files
            .get(&r.file)
            .map(|f| f.module.clone())
            .unwrap_or_default();
        let mut mods = Vec::new();
        let mut cur = r.enclosing.as_ref().and_then(|id| self.symbol(id));
        while let Some(s) = cur {
            if s.kind == SymbolKind::Module {
                mods.push(s.name.clone());
            }
            cur = s.parent.as_ref().and_then(|p| self.symbol(p));
        }
        v.extend(mods.into_iter().rev());
        v
    }
}

/// Caminho que o contêiner de um candidato precisa ter.
#[derive(Debug, Clone)]
struct PathFilter {
    segs: Vec<String>,
    /// Absoluto: o contêiner tem de ser igual. Senão, basta terminar igual.
    anchored: bool,
    /// `crate::` em Rust: o candidato também tem de estar neste crate.
    krate: Option<String>,
}

impl PathFilter {
    fn loose(segs: &[&str]) -> Self {
        Self {
            segs: segs.iter().map(|s| (*s).to_string()).collect(),
            anchored: false,
            krate: None,
        }
    }

    fn anchored(segs: Vec<String>, krate: Option<String>) -> Self {
        Self {
            segs,
            anchored: true,
            krate,
        }
    }

    fn same_crate(&self, file: &str) -> bool {
        self.krate.as_ref().is_none_or(|k| crate_root(file) == *k)
    }

    /// O contêiner de um candidato bate com o hint.
    fn matches(&self, container: &[String], file: &str) -> bool {
        if self.anchored {
            container == self.segs.as_slice() && self.same_crate(file)
        } else {
            let segs: Vec<&str> = self.segs.iter().map(String::as_str).collect();
            ends_with(container, &segs)
        }
    }

    /// O import (este caminho) alcança o candidato de caminho `full`.
    fn covers(&self, full: &[String], file: &str) -> bool {
        if self.anchored {
            full.starts_with(&self.segs) && self.same_crate(file)
        } else {
            !self.segs.is_empty()
                && full
                    .windows(self.segs.len())
                    .any(|w| w == self.segs.as_slice())
        }
    }
}

/// Espécie compatível com o sítio (passo 1).
fn kind_fits(r: &Ref, kind: SymbolKind) -> bool {
    use SymbolKind as K;
    match r.kind {
        RefKind::Use => true,
        RefKind::Type => matches!(
            kind,
            K::Struct | K::Enum | K::Trait | K::Type | K::Class | K::Interface
        ),
        RefKind::Call if r.method => kind == K::Method,
        RefKind::Call if r.qual_hint.is_some() => {
            matches!(kind, K::Fn | K::Method | K::Struct | K::Class)
        }
        RefKind::Call => matches!(kind, K::Fn | K::Struct | K::Class),
    }
}

/// Import não glob cujo nome local (alias ou nome) é `local`.
fn local_import<'a>(imports: &'a [Import], local: &str) -> Option<&'a Import> {
    imports.iter().find(|i| {
        !i.glob
            && i.alias
                .as_deref()
                .or(i.name.as_deref())
                .is_some_and(|n| n == local)
    })
}

/// Origem relativa à pasta do arquivo: `./m`, `../m` (TS/JS), `.m` (Python).
fn is_relative_source(source: &str, syn: Syntax) -> bool {
    match syn {
        Syntax::Ts => source == "." || source.starts_with("./") || source.starts_with("../"),
        Syntax::Py => source.starts_with('.'),
        _ => false,
    }
}

/// Módulo de uma origem TS/JS ou Python, vista do arquivo `file`.
fn module_path(source: &str, file: &str, syn: Syntax) -> PathFilter {
    match syn {
        Syntax::Ts if is_relative_source(source, syn) => {
            let mut segs = dir_segs(file);
            for part in source.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        segs.pop();
                    }
                    p => segs.push(p.to_string()),
                }
            }
            if let Some(last) = segs.last_mut()
                && let Some((stem, ext)) = last.rsplit_once('.')
                && matches!(
                    ext,
                    "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
                )
            {
                *last = stem.to_string();
            }
            if segs.last().is_some_and(|l| l == "index") {
                segs.pop();
            }
            PathFilter::anchored(segs, None)
        }
        Syntax::Py if is_relative_source(source, syn) => {
            let dots = source.chars().take_while(|c| *c == '.').count();
            let mut segs = dir_segs(file);
            for _ in 1..dots {
                segs.pop();
            }
            segs.extend(
                split_nonempty(&source[dots..], ".")
                    .into_iter()
                    .map(String::from),
            );
            PathFilter::anchored(segs, None)
        }
        Syntax::Ts => PathFilter::loose(&split_nonempty(source, "/")),
        Syntax::Rust => PathFilter::loose(&split_nonempty(source, "::")),
        Syntax::Py | Syntax::Other => PathFilter::loose(&split_nonempty(source, ".")),
    }
}

/// Tipo de um `impl` pelo cabeçalho: `Rect` em `impl Shape for Rect`.
fn impl_self_type(name: &str) -> String {
    let ty = match name.rsplit_once(" for ") {
        Some((_, t)) => t,
        None => name.strip_prefix("impl ").unwrap_or(name),
    };
    let ty = ty
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches("dyn ")
        .trim();
    let ty = ty.split('<').next().unwrap_or(ty);
    ty.rsplit("::").next().unwrap_or(ty).trim().to_string()
}

/// Pasta de um arquivo (`src` para `src/a.rs`, `""` na raiz).
fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(d, _)| d)
}

fn dir_segs(file: &str) -> Vec<String> {
    split_nonempty(dir_of(file), "/")
        .into_iter()
        .map(String::from)
        .collect()
}

/// Raiz do crate de um arquivo Rust: o caminho antes do último `src/`.
fn crate_root(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    match dirs.iter().rposition(|p| *p == "src") {
        Some(i) => parts[..i].join("/"),
        None => String::new(),
    }
}

fn split_nonempty<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    s.split(sep).filter(|p| !p.is_empty()).collect()
}

/// `path` termina com os segmentos `segs`.
fn ends_with(path: &[String], segs: &[&str]) -> bool {
    path.len() >= segs.len()
        && path[path.len() - segs.len()..]
            .iter()
            .zip(segs)
            .all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_order_from_near_to_far() {
        assert!(Tier::SameFile < Tier::SameModule);
        assert!(Tier::SameModule < Tier::Imported);
        assert!(Tier::Imported < Tier::Global);
    }

    #[test]
    fn ambiguous_is_tagged_on_the_wire() {
        let a = SymbolId("src/geometry.rs::validate".into());
        let b = SymbolId("src/store.rs::validate".into());
        let r = Resolution::Ambiguous {
            candidates: vec![a.clone(), b.clone()],
            tier: Tier::Global,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({
                "status": "ambiguous",
                "candidates": ["src/geometry.rs::validate", "src/store.rs::validate"],
                "tier": "global"
            })
        );
        assert!(r.mentions(&a) && r.mentions(&b));
        assert!(!Resolution::Unresolved.mentions(&a));
    }

    use crate::context::{Span, SymbolKind as K};

    /// `(qual_name, kind)`; o `name` é o último segmento.
    fn facts(
        file: &str,
        module: &[&str],
        syms: &[(&str, K)],
        imports: &[(&str, &str)],
    ) -> FileFacts {
        let sep = Syntax::of(file).sep();
        let symbols = syms
            .iter()
            .map(|(qual, kind)| {
                let (parent, name) = match qual.rsplit_once(sep) {
                    _ if qual.starts_with("impl ") => (None, *qual),
                    Some((p, n)) => (Some(SymbolId(format!("{file}::{p}"))), n),
                    None => (None, *qual),
                };
                Symbol {
                    id: SymbolId(format!("{file}::{qual}")),
                    name: name.to_string(),
                    qual_name: (*qual).to_string(),
                    kind: *kind,
                    file: file.to_string(),
                    span: Span::default(),
                    signature_span: Span::default(),
                    doc_span: None,
                    parent,
                }
            })
            .collect();
        FileFacts {
            module: module.iter().map(|s| (*s).to_string()).collect(),
            symbols,
            refs: Vec::new(),
            imports: imports
                .iter()
                .map(|(source, name)| Import {
                    source: (*source).to_string(),
                    name: Some((*name).to_string()),
                    alias: None,
                    glob: false,
                    file: file.to_string(),
                    span: Span::default(),
                })
                .collect(),
            parse_errors: 0,
        }
    }

    fn call(file: &str, name: &str, hint: Option<&str>, method: bool, enc: Option<&str>) -> Ref {
        Ref {
            name: name.to_string(),
            qual_hint: hint.map(String::from),
            method,
            file: file.to_string(),
            span: Span::default(),
            enclosing: enc.map(|e| SymbolId(format!("{file}::{e}"))),
            kind: RefKind::Call,
        }
    }

    fn table(files: Vec<(&str, FileFacts)>) -> SymbolTable {
        let mut t = SymbolTable::new();
        for (f, facts) in files {
            t.insert_file(f, facts).unwrap();
        }
        t
    }

    fn resolved(id: &str, tier: Tier) -> Resolution {
        Resolution::Resolved {
            target: SymbolId(id.into()),
            tier,
        }
    }

    /// A forma da fixture TS da P6b: import relativo com extensão, `this`,
    /// `new X()` e método sem tipo de receptor.
    #[test]
    fn ts_relative_imports_and_this() {
        let t = table(vec![
            (
                "src/shapes.ts",
                facts(
                    "src/shapes.ts",
                    &["src", "shapes"],
                    &[
                        ("Shape", K::Interface),
                        ("Shape.area", K::Method),
                        ("Rect", K::Class),
                        ("Rect.area", K::Method),
                        ("Rect.square", K::Method),
                        ("totalArea", K::Fn),
                    ],
                    &[],
                ),
            ),
            (
                "src/legacy.js",
                facts(
                    "src/legacy.js",
                    &["src", "legacy"],
                    &[("format", K::Fn)],
                    &[],
                ),
            ),
            (
                "src/main.ts",
                facts(
                    "src/main.ts",
                    &["src", "main"],
                    &[("run", K::Fn)],
                    &[
                        ("./shapes", "Rect"),
                        ("./shapes", "totalArea"),
                        ("./legacy.js", "format"),
                    ],
                ),
            ),
            (
                "lib/other.ts",
                facts("lib/other.ts", &["lib", "other"], &[("format", K::Fn)], &[]),
            ),
        ]);
        let main = "src/main.ts";
        let shapes = "src/shapes.ts";
        assert_eq!(
            t.resolve_ref(&call(main, "square", Some("Rect"), false, Some("run"))),
            resolved("src/shapes.ts::Rect.square", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call(main, "format", None, false, Some("run"))),
            resolved("src/legacy.js::format", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call(shapes, "Rect", None, false, Some("Rect.square"))),
            resolved("src/shapes.ts::Rect", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call(
                shapes,
                "area",
                Some("this"),
                true,
                Some("Rect.square")
            )),
            resolved("src/shapes.ts::Rect.area", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call(shapes, "area", None, true, Some("totalArea"))),
            Resolution::Ambiguous {
                candidates: vec![
                    SymbolId("src/shapes.ts::Rect.area".into()),
                    SymbolId("src/shapes.ts::Shape.area".into())
                ],
                tier: Tier::SameFile
            }
        );
        // Pacote externo: o filtro zera.
        let mut ext = call(main, "useState", Some("React"), false, Some("run"));
        assert_eq!(t.resolve_ref(&ext), Resolution::Unresolved);
        ext.name = "format".into();
        ext.qual_hint = None;
        ext.file = "lib/x.ts".into();
        assert_eq!(
            t.resolve_ref(&ext),
            resolved("lib/other.ts::format", Tier::SameModule)
        );
    }

    /// A forma da fixture Python da P6b: `from .m import X`, `self.f()`, e
    /// `X()` como chamada da classe.
    #[test]
    fn py_relative_imports_and_self() {
        let t = table(vec![
            (
                "pkg/shapes.py",
                facts(
                    "pkg/shapes.py",
                    &["pkg", "shapes"],
                    &[
                        ("Rect", K::Class),
                        ("Rect.__init__", K::Method),
                        ("Rect.area", K::Method),
                        ("Rect.square", K::Method),
                        ("total_area", K::Fn),
                    ],
                    &[],
                ),
            ),
            (
                "pkg/main.py",
                facts(
                    "pkg/main.py",
                    &["pkg", "main"],
                    &[("run", K::Fn)],
                    &[(".shapes", "Rect"), (".shapes", "total_area")],
                ),
            ),
            (
                "app/cli.py",
                facts(
                    "app/cli.py",
                    &["app", "cli"],
                    &[("main", K::Fn)],
                    &[("pkg.shapes", "Rect"), ("..pkg.shapes", "total_area")],
                ),
            ),
        ]);
        assert_eq!(
            t.resolve_ref(&call(
                "pkg/main.py",
                "square",
                Some("Rect"),
                false,
                Some("run")
            )),
            resolved("pkg/shapes.py::Rect.square", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call("pkg/main.py", "total_area", None, false, Some("run"))),
            resolved("pkg/shapes.py::total_area", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call(
                "pkg/shapes.py",
                "area",
                Some("self"),
                false,
                Some("Rect.square")
            )),
            resolved("pkg/shapes.py::Rect.area", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call(
                "pkg/shapes.py",
                "area",
                None,
                true,
                Some("total_area")
            )),
            resolved("pkg/shapes.py::Rect.area", Tier::SameFile)
        );
        // Import absoluto e relativo de outro pacote: degrau `imported`.
        assert_eq!(
            t.resolve_ref(&call(
                "app/cli.py",
                "square",
                Some("Rect"),
                false,
                Some("main")
            )),
            resolved("pkg/shapes.py::Rect.square", Tier::Imported)
        );
        assert_eq!(
            t.resolve_ref(&call("app/cli.py", "total_area", None, false, Some("main"))),
            resolved("pkg/shapes.py::total_area", Tier::Imported)
        );
    }

    /// `self::`, `super::`, alias de `use`, `self.f()` e `crate::` presos ao
    /// próprio crate.
    #[test]
    fn rust_paths_aliases_and_crates() {
        let mut lib = facts(
            "src/lib.rs",
            &[],
            &[
                ("helper", K::Fn),
                ("inner", K::Module),
                ("inner::helper", K::Fn),
                ("inner::run", K::Fn),
                ("S", K::Struct),
                ("impl S", K::Impl),
                ("S::go", K::Method),
                ("S::step", K::Method),
            ],
            &[],
        );
        for s in &mut lib.symbols {
            if s.qual_name.starts_with("S::") {
                s.parent = Some(SymbolId("src/lib.rs::impl S".into()));
            }
        }
        let mut alias = facts("src/a.rs", &["a"], &[("run", K::Fn)], &[]);
        alias.imports.push(Import {
            source: "crate::inner".into(),
            name: Some("helper".into()),
            alias: Some("h".into()),
            glob: false,
            file: "src/a.rs".into(),
            span: Span::default(),
        });
        let t = table(vec![
            ("src/lib.rs", lib),
            ("src/a.rs", alias),
            (
                "crates/other/src/lib.rs",
                facts("crates/other/src/lib.rs", &[], &[("helper", K::Fn)], &[]),
            ),
        ]);
        let lib = "src/lib.rs";
        assert_eq!(
            t.resolve_ref(&call(
                lib,
                "helper",
                Some("self"),
                false,
                Some("inner::run")
            )),
            resolved("src/lib.rs::inner::helper", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call(
                lib,
                "helper",
                Some("super"),
                false,
                Some("inner::run")
            )),
            resolved("src/lib.rs::helper", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call(lib, "step", Some("self"), true, Some("S::go"))),
            resolved("src/lib.rs::S::step", Tier::SameFile)
        );
        assert_eq!(
            t.resolve_ref(&call("src/a.rs", "h", None, false, Some("run"))),
            resolved("src/lib.rs::inner::helper", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call(
                "src/a.rs",
                "helper",
                Some("crate"),
                false,
                Some("run")
            )),
            resolved("src/lib.rs::helper", Tier::SameModule)
        );
        assert_eq!(
            t.resolve_ref(&call(
                "crates/other/src/lib.rs",
                "helper",
                Some("crate"),
                false,
                None
            )),
            resolved("crates/other/src/lib.rs::helper", Tier::SameFile)
        );
        // Sem hint nem import, empata entre os dois arquivos.
        assert!(matches!(
            t.resolve_ref(&call("tests/t.rs", "helper", None, false, None)),
            Resolution::Ambiguous { candidates, tier: Tier::Global } if candidates.len() == 3
        ));
        // `sites` acha o sítio do alias pelo nome original.
        let mut t = t;
        let mut a = t.file("src/a.rs").unwrap().clone();
        a.refs.push(call("src/a.rs", "h", None, false, Some("run")));
        t.insert_file("src/a.rs", a).unwrap();
        let hits = t.sites(&[SymbolId("src/lib.rs::inner::helper".into())], true);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.name, "h");
    }

    #[test]
    fn lookup_by_name_suffix_and_id() {
        let t = table(vec![(
            "src/lib.rs",
            facts(
                "src/lib.rs",
                &[],
                &[("impl fmt::Display for S", K::Impl), ("inner::f", K::Fn)],
                &[],
            ),
        )]);
        let one = |q: &str| t.lookup(q).unwrap();
        assert_eq!(
            t.symbol(&one("impl fmt::Display for S")[0]).unwrap().name,
            "impl fmt::Display for S"
        );
        assert!(one("Display for S").is_empty());
        assert_eq!(one("inner::f"), one("src/lib.rs::inner::f"));
        assert_eq!(one("f").len(), 1);
        assert!(one("other::f").is_empty());
        assert!(one("src/lib.rs::f").is_empty());
        assert!(one("  ").is_empty());
    }
}
