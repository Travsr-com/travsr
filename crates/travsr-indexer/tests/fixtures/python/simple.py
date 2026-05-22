"""Simple Python fixture for tree-sitter indexer tests."""

import os
import sys
from pathlib import Path
from typing import Optional, List

CONSTANT = 42
_PRIVATE = "hidden"


def top_level_fn(x: int) -> int:
    return x + 1


def helper() -> None:
    pass


class Animal:
    species: str = "unknown"

    def __init__(self, name: str) -> None:
        self.name = name

    def speak(self) -> str:
        return f"{self.name} says hello"

    @staticmethod
    def kingdom() -> str:
        return "Animalia"

    @classmethod
    def create(cls, name: str) -> "Animal":
        return cls(name)


class Dog(Animal):
    def speak(self) -> str:
        return f"{self.name} barks"

    def fetch(self, item: str) -> str:
        return f"{self.name} fetches {item}"


def process(items: List[int]) -> Optional[int]:
    if not items:
        return None
    return sum(items)
