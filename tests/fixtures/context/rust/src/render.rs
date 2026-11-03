//! Desenho em texto: chama `geometry` e `store` (chamadas entre módulos).

use crate::geometry::Rect;
use crate::store::{Store, StoreError};

/// Monta uma coleção com `n` quadrados de lado crescente.
pub fn build(n: u32) -> Result<Store, StoreError> {
    let mut store = Store::new();
    for i in 1..=n {
        store.add(Rect::square(f64::from(i)))?;
    }
    Ok(store)
}

/// Descreve um retângulo, recusando dimensões inválidas.
pub fn describe(r: &Rect) -> String {
    if crate::geometry::validate(r) {
        format!("{}x{}", r.w, r.h)
    } else {
        String::from("inválido")
    }
}

/// Primeiro retângulo de exemplo.
pub fn sample() -> Rect {
    Rect::new(2.0, 3.0)
}
