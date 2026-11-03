"""Formas geométricas."""


class Rect:
    """Retângulo alinhado aos eixos."""

    def __init__(self, w: float, h: float) -> None:
        self.w = w
        self.h = h

    def area(self) -> float:
        """Área em unidades quadradas."""
        return self.w * self.h

    @staticmethod
    def square(side: float) -> "Rect":
        return Rect(side, side)


def total_area(shapes: list[Rect]) -> float:
    """Soma as áreas."""
    return sum(s.area() for s in shapes)
