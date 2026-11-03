// Formas para o kax-context (ADR 0009, P6b).

/** Algo que tem área. */
export interface Shape {
  area(): number;
}

/** Retângulo alinhado aos eixos. */
export class Rect implements Shape {
  constructor(public w: number, public h: number) {}

  /** Área em unidades quadradas. */
  area(): number {
    return this.w * this.h;
  }

  static square(side: number): Rect {
    return new Rect(side, side);
  }
}

/** Soma as áreas. */
export function totalArea(shapes: Shape[]): number {
  return shapes.reduce((acc, s) => acc + s.area(), 0);
}
