import { Rect, totalArea } from "./shapes";
import { format } from "./legacy.js";

/** Área de um quadrado de lado 2, formatada. */
export function run(): string {
  const r = Rect.square(2);
  return format(totalArea([r]));
}
