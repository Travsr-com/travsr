"""Nested module exercising multi-level relative imports (INDEX-222)."""
from ..utils import helper


def deep_fn() -> str:
    return helper()
