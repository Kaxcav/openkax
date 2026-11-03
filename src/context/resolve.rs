//! Resolução heurística de referências (ADR 0009, decisão 5). Lógica: P6c.
//!
//! Algoritmo que a P6c implementa:
//! 1. **Candidatos**: definições com `name == ref.name` e espécie compatível:
//!    `call` → fn, struct, class (construtor); `call` com `method` → method;
//!    `type` → struct, enum, trait, type, class, interface; `use` → qualquer.
//! 2. **Hint**: com `qual_hint`, normaliza `self`/`Self`/`this` (tipo do
//!    contêiner do sítio), `crate`/`super` (módulo do arquivo) e expande um
//!    alias de import no 1º segmento; fica quem tem o hint como sufixo do
//!    caminho completo (`module` + segmentos do `qual_name`). Sem `qual_hint`,
//!    um import explícito do nome no arquivo filtra pelo caminho do import.
//!    Filtro que zera → [`Resolution::Unresolved`] (nome externo).
//! 3. **Proximidade** ([`Tier`]): mesmo arquivo > mesmo módulo (mesmo
//!    diretório) > importado (import explícito, glob ou de módulo) > global.
//!    O degrau de um candidato é o mais perto que se aplica a ele: um item
//!    importado de um arquivo vizinho fica em `SameModule`.
//! 4. No melhor degrau com candidatos: 1 → [`Resolution::Resolved`];
//!    vários → [`Resolution::Ambiguous`]. Nunca escolhe entre empatados.

use serde::{Deserialize, Serialize};

use super::{ContextError, FileFacts, Ref, SymbolId};

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

/// Tabela global de definições e imports, atualizada arquivo a arquivo (um
/// arquivo alterado sai e entra de novo, sem reconstruir o resto).
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    files: std::collections::BTreeMap<String, FileFacts>,
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

    /// Insere (ou troca) os fatos de `file`.
    pub fn insert_file(&mut self, _file: &str, _facts: FileFacts) -> Result<(), ContextError> {
        Err(ContextError::NotImplemented(
            "P6c: SymbolTable::insert_file",
        ))
    }

    /// Tira `file` e tudo o que ele definia.
    pub fn remove_file(&mut self, _file: &str) -> Result<(), ContextError> {
        Err(ContextError::NotImplemented(
            "P6c: SymbolTable::remove_file",
        ))
    }

    /// Definições que casam com a query (nome, caminho qualificado por sufixo
    /// ou id), em ordem de id.
    pub fn lookup(&self, _query: &str) -> Result<Vec<SymbolId>, ContextError> {
        Err(ContextError::NotImplemented("P6c: SymbolTable::lookup"))
    }

    /// Liga um sítio a uma definição (algoritmo no topo do módulo).
    pub fn resolve(&self, _r: &Ref) -> Result<Resolution, ContextError> {
        Err(ContextError::NotImplemented("P6c: SymbolTable::resolve"))
    }
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
}
