"""Parsing queries: words, "quoted phrases" and -exclusions.

Query text goes through the same analyzer as document text, so folding, word splitting and stopwords
are the same on both sides, and phrase offsets are positions among terms, as in the index.
"""

import re
from dataclasses import dataclass, field

from findit.analyzer import analyze, terms

_QUOTED = re.compile(r'"([^"]*)"?')


@dataclass
class Query:
    terms: list = field(default_factory=list)
    # Each phrase is a list of (offset, term): where each term must sit relative to the phrase's start.
    phrases: list = field(default_factory=list)
    excluded: list = field(default_factory=list)

    def looked_for(self):
        """Every term the query looks for: its words, then the terms of its phrases."""
        out = list(self.terms)
        for phrase in self.phrases:
            out.extend(term for _, term in phrase)
        return out

    def is_empty(self):
        return not self.terms and not self.phrases


def parse(text):
    query = Query()
    for match in _QUOTED.finditer(text):
        phrase = analyze(match.group(1))
        if phrase:
            query.phrases.append(phrase)
    rest = _QUOTED.sub(" ", text)
    for word in rest.split():
        if word.startswith("-") and len(word) > 1:
            query.excluded.extend(terms(word[1:]))
            continue
        query.terms.extend(terms(word))
    return query
