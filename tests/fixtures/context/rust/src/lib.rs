//! Crate de fixture do kax-context (ADR 0009, P6a).
//!
//! Pequeno de propósito: struct, enum, trait, impl, fn livre, método, chamada
//! entre módulos, um nome repetido em dois módulos e doc comments.

pub mod geometry;
pub mod render;
pub mod store;

pub use geometry::Shape;

/// Soma as áreas de todas as formas.
pub fn total_area(shapes: &[Box<dyn Shape>]) -> f64 {
    shapes.iter().map(|s| s.area()).sum()
}
