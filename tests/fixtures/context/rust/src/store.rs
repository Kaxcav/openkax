//! Armazenamento de retângulos.

use crate::geometry::{Rect, Shape};

/// Limite de itens guardados.
pub const MAX_ITEMS: usize = 64;

/// Por que um item foi recusado.
#[derive(Debug, PartialEq)]
pub enum StoreError {
    Full,
    Invalid,
}

/// Coleção de retângulos válidos.
#[derive(Default)]
pub struct Store {
    items: Vec<Rect>,
}

impl Store {
    pub fn new() -> Self {
        Store { items: Vec::new() }
    }

    /// Guarda `r` se houver espaço e as dimensões forem válidas.
    pub fn add(&mut self, r: Rect) -> Result<(), StoreError> {
        if self.items.len() >= MAX_ITEMS {
            return Err(StoreError::Full);
        }
        if !crate::geometry::validate(&r) {
            return Err(StoreError::Invalid);
        }
        self.items.push(r);
        Ok(())
    }

    /// Área somada dos itens.
    pub fn area(&self) -> f64 {
        self.items.iter().map(Shape::area).sum()
    }
}

/// Coleção dentro do limite. Mesmo nome de `geometry::validate`, de propósito.
pub fn validate(store: &Store) -> bool {
    store.items.len() <= MAX_ITEMS
}
