//! Peças comuns aos adapters: tabela de linhas e spans, contagem de nós de
//! erro e ids únicos por arquivo (ADR 0009, decisão 3).

use std::collections::HashSet;
use std::path::Path;

use tree_sitter::Node;

use crate::context::{Span, SymbolId};

/// Caminho relativo com `/`, como vai para os campos `file`.
pub(crate) fn path_str(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Início de cada linha do texto, para converter byte em linha.
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(src: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        Self { starts }
    }

    /// Linha 1-based do byte.
    pub(crate) fn line(&self, byte: usize) -> u32 {
        self.starts.partition_point(|&s| s <= byte) as u32
    }

    /// Span de `[start, end)`. A linha final é a do último byte: o `\n` que
    /// fecha um comentário de linha não conta como linha seguinte.
    pub(crate) fn span(&self, start: usize, end: usize) -> Span {
        let last = if end > start { end - 1 } else { start };
        Span {
            start_byte: start as u32,
            end_byte: end as u32,
            start_line: self.line(start),
            end_line: self.line(last),
        }
    }
}

/// Nós `ERROR` e `MISSING` da árvore. Só desce nas subárvores com erro.
pub(crate) fn count_errors(root: Node<'_>) -> u32 {
    if !root.has_error() {
        return 0;
    }
    let mut n = 0;
    let mut cursor = root.walk();
    'tree: loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            n += 1;
        }
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'tree;
            }
            if !cursor.goto_parent() {
                break 'tree;
            }
        }
    }
    n
}

/// Gera `<arquivo>::<qual_name>`; na colisão, o 1º fica sem sufixo e os
/// seguintes ganham `#<linha inicial>`.
#[derive(Default)]
pub(crate) struct Ids(HashSet<String>);

impl Ids {
    pub(crate) fn make(&mut self, file: &str, qual: &str, line: u32) -> SymbolId {
        let base = format!("{file}::{qual}");
        if self.0.insert(base.clone()) {
            return SymbolId(base);
        }
        let mut id = format!("{base}#{line}");
        let mut n = 2;
        while !self.0.insert(id.clone()) {
            id = format!("{base}#{line}.{n}");
            n += 1;
        }
        SymbolId(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_ignore_the_trailing_newline() {
        let li = LineIndex::new("a\nbc\n\nd");
        assert_eq!(li.line(0), 1);
        assert_eq!(li.line(2), 2);
        assert_eq!(li.line(5), 3);
        assert_eq!(li.line(6), 4);
        assert_eq!(li.span(2, 5).lines(), [2, 2]);
        assert_eq!(li.span(2, 6).lines(), [2, 3]);
        assert_eq!(li.span(6, 6).lines(), [4, 4]);
    }

    #[test]
    fn ids_get_the_line_on_collision() {
        let mut ids = Ids::default();
        assert_eq!(ids.make("a.rs", "f", 1).0, "a.rs::f");
        assert_eq!(ids.make("a.rs", "f", 9).0, "a.rs::f#9");
        assert_eq!(ids.make("a.rs", "f", 9).0, "a.rs::f#9.2");
    }
}
