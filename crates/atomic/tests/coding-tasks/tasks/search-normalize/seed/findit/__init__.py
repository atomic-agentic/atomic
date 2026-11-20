"""findit: a small full-text search over a folder of text files."""

from findit.index import Index
from findit.search import search

__all__ = ["Index", "search"]
