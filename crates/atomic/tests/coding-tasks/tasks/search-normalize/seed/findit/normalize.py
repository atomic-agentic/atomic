"""Folding: one form for a word, whatever its case and accents."""

import unicodedata


def fold(word):
    """NFKD, without combining marks, case-folded: 'Café' -> 'cafe', 'Straße' -> 'strasse'."""
    decomposed = unicodedata.normalize("NFKD", word)
    bare = "".join(c for c in decomposed if not unicodedata.combining(c))
    return bare.casefold()
