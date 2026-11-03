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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ContextError, FileFacts, Ref, Symbol, SymbolId, adapter_for};

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
    pub fn resolve(&self, _r: &Ref) -> Result<Resolution, ContextError> {
        Err(ContextError::NotImplemented("P6c: SymbolTable::resolve"))
    }
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
}
