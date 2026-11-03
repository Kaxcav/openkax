//! Tipos geométricos.

/// Algo que tem área.
pub trait Shape {
    /// Área em unidades quadradas.
    fn area(&self) -> f64;
}

/// Retângulo alinhado aos eixos.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// Cria um retângulo.
    pub fn new(w: f64, h: f64) -> Self {
        Rect { w, h }
    }

    /// Quadrado de lado `side`.
    pub fn square(side: f64) -> Self {
        Self::new(side, side)
    }
}

impl Shape for Rect {
    fn area(&self) -> f64 {
        self.w * self.h
    }
}

/// Dimensões positivas. Mesmo nome de `store::validate`, de propósito.
pub fn validate(r: &Rect) -> bool {
    r.w > 0.0 && r.h > 0.0
}
