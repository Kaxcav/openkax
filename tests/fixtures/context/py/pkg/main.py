"""Usa as formas: chamadas entre módulos."""

from .shapes import Rect, total_area


def run() -> float:
    r = Rect.square(2.0)
    return total_area([r])
