//! Empacotamento com orçamento de tokens (ADR 0009, decisão 6). Lógica: P6c.
//!
//! Prioridade, gulosa nessa ordem; dentro de cada uma, na ordem em que o
//! símbolo aparece (o tipo dono primeiro, depois as refs dos alvos na ordem
//! do fonte, e os chamadores por arquivo e linha):
//! 1. [`PackRole::Target`]: corpo dos alvos (doc + `span`), obrigatório;
//! 2. [`PackRole::TypeDef`]: definição dos tipos usados pelos alvos,
//!    inclusive o tipo dono de um método alvo;
//! 3. [`PackRole::Callee`]: assinatura das funções chamadas pelos alvos;
//! 4. [`PackRole::Caller`]: assinatura dos chamadores dos alvos.
//!
//! Item que não cabe → [`OmitReason::Budget`] e segue (um menor pode caber);
//! dependência ambígua → [`OmitReason::Ambiguous`]; trecho contido em outro já
//! incluído não repete. Alvos que não cabem →
//! [`ContextError::BudgetTooSmall`]. Tokens = `ceil(chars / chars_per_token)`,
//! a mesma conta do scheduler.

use serde::{Deserialize, Serialize};

use super::{ContextError, SymbolId, SymbolTable};

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
    _table: &SymbolTable,
    _source: &dyn Fn(&str) -> Result<String, ContextError>,
    _targets: &[SymbolId],
    _budget_tokens: u32,
    _chars_per_token: f64,
) -> Result<ContextPack, ContextError> {
    Err(ContextError::NotImplemented("P6c: pack::build"))
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
