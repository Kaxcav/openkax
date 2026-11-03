//! Empacotamento com orçamento de tokens (ADR 0009, decisão 6).
//!
//! Prioridade, gulosa nessa ordem; dentro de cada uma, na ordem em que o
//! símbolo aparece (o tipo dono primeiro, depois as refs dos alvos na ordem
//! do fonte, e os chamadores por arquivo e linha):
//! 1. [`PackRole::Target`]: corpo dos alvos (doc + `span`), obrigatório;
//! 2. [`PackRole::TypeDef`]: definição dos tipos usados pelos alvos,
//!    inclusive o tipo dono de um método alvo e o tipo construído por uma
//!    chamada (`new X()`, `X()`);
//! 3. [`PackRole::Callee`]: assinatura das funções chamadas pelos alvos;
//! 4. [`PackRole::Caller`]: assinatura dos chamadores dos alvos.
//!
//! Item que não cabe → [`OmitReason::Budget`] e segue (um menor pode caber);
//! dependência ambígua → [`OmitReason::Ambiguous`] (um chamador cujo sítio é
//! ambíguo também, com o id dele); trecho contido em outro já incluído não
//! repete; definição inteira que conteria um trecho já incluído (a classe de
//! um método alvo) entra só pela assinatura. Alvos que não cabem →
//! [`ContextError::BudgetTooSmall`]. Tokens = `ceil(chars / chars_per_token)`,
//! a mesma conta do scheduler; `chars_per_token` não positivo vale 1.
//!
//! No fim, trechos `full` do mesmo arquivo que se tocam ou se sobrepõem viram
//! um só, com o papel e o símbolo do de maior prioridade e o texto das linhas
//! somadas.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::resolve::impl_self_type;
use super::{
    ContextError, Ref, RefKind, Resolution, Symbol, SymbolId, SymbolKind, SymbolTable, Tier,
};

/// Por que o trecho entrou no pacote (também é a prioridade, em ordem).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackRole {
    Target,
    TypeDef,
    Callee,
    Caller,
}

/// Quanto do símbolo foi incluído.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Doc + item inteiro.
    Full,
    /// Só o `signature_span`.
    Signature,
}

/// Trecho incluído.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excerpt {
    pub file: String,
    /// `[início, fim]`, 1-based e inclusivo.
    pub lines: [u32; 2],
    pub symbol: SymbolId,
    pub role: PackRole,
    pub fidelity: Fidelity,
    /// `Full`: as linhas `lines` inteiras, cada uma com `\n`. `Signature`: o
    /// texto exato do `signature_span`.
    pub text: String,
    /// `ceil(chars(text) / chars_per_token)`.
    pub tokens_estimated: u32,
}

impl Excerpt {
    /// `arquivo:início-fim`, a forma usada no pacote.
    pub fn location(&self) -> String {
        format!("{}:{}-{}", self.file, self.lines[0], self.lines[1])
    }
}

/// Motivo de um símbolo ter ficado fora.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum OmitReason {
    /// Não coube no que sobrava do orçamento.
    Budget { tokens_estimated: u32 },
    /// A referência empatou entre candidatos; nenhum foi escolhido.
    Ambiguous { candidates: Vec<SymbolId> },
}

/// Símbolo que ficou fora, com o motivo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omitted {
    /// Para `Ambiguous`, o nome do sítio (não há um id só).
    pub symbol: String,
    pub role: PackRole,
    #[serde(flatten)]
    pub reason: OmitReason,
}

/// Contexto enxuto pronto para o pacote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPack {
    /// Saída em ordem de prioridade e, dentro dela, de arquivo e linha (a
    /// ordem de escolha é a do topo do módulo).
    pub excerpts: Vec<Excerpt>,
    pub omitted: Vec<Omitted>,
    /// Soma dos `tokens_estimated` dos trechos; nunca passa de `budget_tokens`.
    pub tokens_estimated: u32,
    pub budget_tokens: u32,
}

/// Monta o pacote de `targets` (ids já resolvidos e sem ambiguidade) lendo o
/// texto de `source(file)`.
pub fn build(
    table: &SymbolTable,
    source: &dyn Fn(&str) -> Result<String, ContextError>,
    targets: &[SymbolId],
    budget_tokens: u32,
    chars_per_token: f64,
) -> Result<ContextPack, ContextError> {
    let cpt = if chars_per_token.is_finite() && chars_per_token > 0.0 {
        chars_per_token
    } else {
        1.0
    };
    let mut p = Packer {
        table,
        source,
        texts: HashMap::new(),
        cpt,
        budget: budget_tokens,
        used: 0,
        chosen: Vec::new(),
        omitted: Vec::new(),
        seen: HashSet::new(),
        seen_ambiguous: HashSet::new(),
    };

    // 1. Alvos: sem repetir e sem um que já está dentro de outro.
    let mut syms: Vec<&Symbol> = Vec::new();
    for id in targets {
        let s = table
            .symbol(id)
            .ok_or_else(|| ContextError::UnknownSymbol(id.to_string()))?;
        if !syms.iter().any(|x| x.id == s.id) {
            syms.push(s);
        }
    }
    let ranges: Vec<[u32; 2]> = syms.iter().map(|s| full_lines(s)).collect();
    let kept: Vec<&Symbol> = syms
        .iter()
        .enumerate()
        .filter(|&(i, s)| {
            !syms.iter().enumerate().any(|(j, o)| {
                j != i
                    && o.file == s.file
                    && contains(ranges[j], ranges[i])
                    && (ranges[j] != ranges[i] || j < i)
            })
        })
        .map(|(_, s)| *s)
        .collect();
    let mut needed = 0u32;
    for s in &kept {
        p.seen.insert(s.id.clone());
        let ex = p.excerpt(s, PackRole::Target, Fidelity::Full)?;
        needed = needed.saturating_add(ex.tokens_estimated);
        p.chosen.push(ex);
    }
    if needed > budget_tokens {
        return Err(ContextError::BudgetTooSmall {
            needed,
            budget: budget_tokens,
        });
    }
    p.used = needed;

    // Refs dentro dos alvos, em ordem do fonte, resolvidas uma vez.
    let mut deps: Vec<(&Ref, Resolution)> = Vec::new();
    for s in &kept {
        let Some(facts) = table.file(&s.file) else {
            continue;
        };
        let mut inside: Vec<&Ref> = facts
            .refs
            .iter()
            .filter(|r| {
                r.span.start_byte >= s.span.start_byte && r.span.end_byte <= s.span.end_byte
            })
            .collect();
        inside.sort_by_key(|r| r.span.start_byte);
        deps.extend(inside.into_iter().map(|r| (r, table.resolve_ref(r))));
    }
    let is_type = |res: &Resolution| match res {
        Resolution::Resolved { target, .. } => {
            table.symbol(target).is_some_and(|s| is_type_kind(s.kind))
        }
        _ => false,
    };

    // 2. Tipos: o dono de cada alvo, depois os usados.
    for s in &kept {
        if let Some((name, res)) = owner(table, s) {
            p.offer_resolution(&res, &name, PackRole::TypeDef)?;
        }
    }
    for (r, res) in &deps {
        if r.kind == RefKind::Type || (r.kind == RefKind::Call && is_type(res)) {
            p.offer_resolution(res, &r.name, PackRole::TypeDef)?;
        }
    }

    // 3. Chamados.
    for (r, res) in &deps {
        if r.kind == RefKind::Call && !is_type(res) {
            p.offer_resolution(res, &r.name, PackRole::Callee)?;
        }
    }

    // 4. Chamadores, por arquivo e linha.
    let ids: Vec<SymbolId> = kept.iter().map(|s| s.id.clone()).collect();
    for (r, res) in table.sites(&ids, true) {
        let Some(caller) = r.enclosing.as_ref().and_then(|id| table.symbol(id)) else {
            continue;
        };
        match res {
            Resolution::Resolved { .. } => {
                p.offer(caller, PackRole::Caller, Fidelity::Signature)?
            }
            Resolution::Ambiguous { candidates, .. } => {
                if !p.inside_chosen(&caller.file, caller.signature_span.lines()) {
                    p.omit_ambiguous(caller.id.to_string(), PackRole::Caller, candidates);
                }
            }
            Resolution::Unresolved => {}
        }
    }

    p.finish()
}

/// Tipo dono de um método (ou associado): a classe/trait que o contém ou o
/// tipo do `impl`, resolvido como uma ref de tipo no cabeçalho do `impl`.
fn owner(table: &SymbolTable, s: &Symbol) -> Option<(String, Resolution)> {
    let parent = s.parent.as_ref().and_then(|p| table.symbol(p))?;
    if is_type_kind(parent.kind) {
        return Some((
            parent.name.clone(),
            Resolution::Resolved {
                target: parent.id.clone(),
                tier: Tier::SameFile,
            },
        ));
    }
    if parent.kind != SymbolKind::Impl {
        return None;
    }
    let ty = impl_self_type(&parent.name);
    let header = table.file(&parent.file).and_then(|f| {
        f.refs.iter().find(|r| {
            r.kind == RefKind::Type
                && r.name == ty
                && r.enclosing.as_ref() == Some(&parent.id)
                && r.span.end_byte <= parent.signature_span.end_byte
        })
    });
    let res = match header {
        Some(r) => table.resolve_ref(r),
        None => table.resolve_ref(&Ref {
            name: ty.clone(),
            qual_hint: None,
            method: false,
            file: parent.file.clone(),
            span: parent.signature_span,
            enclosing: Some(parent.id.clone()),
            kind: RefKind::Type,
        }),
    };
    Some((ty, res))
}

fn is_type_kind(k: SymbolKind) -> bool {
    matches!(
        k,
        SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Type
            | SymbolKind::Class
            | SymbolKind::Interface
    )
}

/// Linhas do trecho `full`: da doc (se houver) ao fim do item.
fn full_lines(s: &Symbol) -> [u32; 2] {
    let start = s.doc_span.map_or(s.span.start_line, |d| d.start_line);
    [start.min(s.span.start_line), s.span.end_line]
}

/// `outer` contém `inner` (linhas, inclusivo).
fn contains(outer: [u32; 2], inner: [u32; 2]) -> bool {
    outer[0] <= inner[0] && inner[1] <= outer[1]
}

fn tokens(text: &str, cpt: f64) -> u32 {
    (text.chars().count() as f64 / cpt).ceil() as u32
}

/// Linhas `[a, b]` de `src`, cada uma com `\n`.
fn whole_lines(src: &str, [a, b]: [u32; 2]) -> String {
    src.lines()
        .skip(a.saturating_sub(1) as usize)
        .take((b + 1).saturating_sub(a) as usize)
        .map(|l| format!("{l}\n"))
        .collect()
}

struct Packer<'a> {
    table: &'a SymbolTable,
    source: &'a dyn Fn(&str) -> Result<String, ContextError>,
    texts: HashMap<String, String>,
    cpt: f64,
    budget: u32,
    used: u32,
    chosen: Vec<Excerpt>,
    omitted: Vec<Omitted>,
    /// Símbolos já oferecidos (incluídos ou omitidos), em qualquer papel.
    seen: HashSet<SymbolId>,
    /// `(nome, papel)` já listados como ambíguos.
    seen_ambiguous: HashSet<(String, PackRole)>,
}

impl Packer<'_> {
    fn text(&mut self, file: &str) -> Result<&str, ContextError> {
        if !self.texts.contains_key(file) {
            let t = (self.source)(file)?;
            self.texts.insert(file.to_string(), t);
        }
        Ok(self.texts[file].as_str())
    }

    fn excerpt(
        &mut self,
        s: &Symbol,
        role: PackRole,
        fidelity: Fidelity,
    ) -> Result<Excerpt, ContextError> {
        let src = self.text(&s.file)?;
        let (lines, text) = match fidelity {
            Fidelity::Full => {
                let lines = full_lines(s);
                (lines, whole_lines(src, lines))
            }
            Fidelity::Signature => (
                s.signature_span.lines(),
                super::index::slice(src, s.signature_span).to_string(),
            ),
        };
        let tokens_estimated = tokens(&text, self.cpt);
        Ok(Excerpt {
            file: s.file.clone(),
            lines,
            symbol: s.id.clone(),
            role,
            fidelity,
            text,
            tokens_estimated,
        })
    }

    fn inside_chosen(&self, file: &str, lines: [u32; 2]) -> bool {
        self.chosen
            .iter()
            .any(|e| e.file == file && contains(e.lines, lines))
    }

    fn offer_resolution(
        &mut self,
        res: &Resolution,
        name: &str,
        role: PackRole,
    ) -> Result<(), ContextError> {
        match res {
            Resolution::Resolved { target, .. } => {
                let Some(s) = self.table.symbol(target) else {
                    return Ok(());
                };
                let fidelity = if role == PackRole::TypeDef {
                    Fidelity::Full
                } else {
                    Fidelity::Signature
                };
                self.offer(s, role, fidelity)
            }
            Resolution::Ambiguous { candidates, .. } => {
                self.omit_ambiguous(name.to_string(), role, candidates.clone());
                Ok(())
            }
            Resolution::Unresolved => Ok(()),
        }
    }

    fn omit_ambiguous(&mut self, symbol: String, role: PackRole, candidates: Vec<SymbolId>) {
        if self.seen_ambiguous.insert((symbol.clone(), role)) {
            self.omitted.push(Omitted {
                symbol,
                role,
                reason: OmitReason::Ambiguous { candidates },
            });
        }
    }

    fn offer(&mut self, s: &Symbol, role: PackRole, want: Fidelity) -> Result<(), ContextError> {
        if !self.seen.insert(s.id.clone()) {
            return Ok(());
        }
        let mut fidelity = want;
        if fidelity == Fidelity::Full {
            let lines = full_lines(s);
            if self.inside_chosen(&s.file, lines) {
                return Ok(());
            }
            // Conteria um trecho já incluído (a classe do método alvo).
            if self
                .chosen
                .iter()
                .any(|e| e.file == s.file && contains(lines, e.lines))
            {
                fidelity = Fidelity::Signature;
            }
        }
        if fidelity == Fidelity::Signature && self.inside_chosen(&s.file, s.signature_span.lines())
        {
            return Ok(());
        }
        let ex = self.excerpt(s, role, fidelity)?;
        if ex.tokens_estimated <= self.budget - self.used {
            self.used += ex.tokens_estimated;
            self.chosen.push(ex);
        } else {
            self.omitted.push(Omitted {
                symbol: s.id.to_string(),
                role,
                reason: OmitReason::Budget {
                    tokens_estimated: ex.tokens_estimated,
                },
            });
        }
        Ok(())
    }

    /// Funde os `full` que se tocam no mesmo arquivo e ordena a saída.
    fn finish(mut self) -> Result<ContextPack, ContextError> {
        let mut chosen = std::mem::take(&mut self.chosen);
        chosen.sort_by(|a, b| (&a.file, a.lines[0]).cmp(&(&b.file, b.lines[0])));
        let mut merged: Vec<Excerpt> = Vec::with_capacity(chosen.len());
        for e in chosen {
            if let Some(prev) = merged.last_mut()
                && prev.file == e.file
                && prev.fidelity == Fidelity::Full
                && e.fidelity == Fidelity::Full
                && e.lines[0] <= prev.lines[1] + 1
            {
                if e.role < prev.role {
                    prev.role = e.role;
                    prev.symbol = e.symbol;
                }
                prev.lines[1] = prev.lines[1].max(e.lines[1]);
                let text = whole_lines(self.text(&prev.file)?, prev.lines);
                prev.tokens_estimated = tokens(&text, self.cpt);
                prev.text = text;
                continue;
            }
            merged.push(e);
        }
        merged.sort_by(|a, b| (a.role, &a.file, a.lines[0]).cmp(&(b.role, &b.file, b.lines[0])));
        let tokens_estimated = merged.iter().map(|e| e.tokens_estimated).sum();
        Ok(ContextPack {
            excerpts: merged,
            omitted: self.omitted,
            tokens_estimated,
            budget_tokens: self.budget,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_order_is_the_priority() {
        assert!(PackRole::Target < PackRole::TypeDef);
        assert!(PackRole::TypeDef < PackRole::Callee);
        assert!(PackRole::Callee < PackRole::Caller);
    }

    #[test]
    fn excerpt_location_and_omitted_wire_shape() {
        let e = Excerpt {
            file: "src/store.rs".into(),
            lines: [21, 30],
            symbol: SymbolId("src/store.rs::Store::add".into()),
            role: PackRole::Target,
            fidelity: Fidelity::Full,
            text: String::new(),
            tokens_estimated: 0,
        };
        assert_eq!(e.location(), "src/store.rs:21-30");
        let o = Omitted {
            symbol: "src/render.rs::draw".into(),
            role: PackRole::Caller,
            reason: OmitReason::Budget {
                tokens_estimated: 40,
            },
        };
        assert_eq!(
            serde_json::to_value(&o).unwrap(),
            serde_json::json!({
                "symbol": "src/render.rs::draw",
                "role": "caller",
                "reason": "budget",
                "tokens_estimated": 40
            })
        );
    }
}
