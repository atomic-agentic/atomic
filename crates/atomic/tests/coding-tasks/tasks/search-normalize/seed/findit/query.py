"""Parsing queries: words, "quoted phrases" and -exclusions."""

import re
import string
from dataclasses import dataclass, field

from findit.stopwords import is_stopword

_QUOTED = re.compile(r'"([^"]*)"?')
_EDGES = string.punctuation + "“”‘’«»"


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


def _term(word):
    return word.strip(_EDGES).lower()


def _phrase(text):
    phrase = []
    for offset, word in enumerate(text.split()):
        term = _term(word)
        if term and not is_stopword(term):
            phrase.append((offset, term))
    return phrase


def parse(text):
    query = Query()
    for match in _QUOTED.finditer(text):
        phrase = _phrase(match.group(1))
        if phrase:
            query.phrases.append(phrase)
    rest = _QUOTED.sub(" ", text)
    for word in rest.split():
        if word.startswith("-") and len(word) > 1:
            term = _term(word[1:])
            if term and not is_stopword(term):
                query.excluded.append(term)
            continue
        term = _term(word)
        if term and not is_stopword(term):
            query.terms.append(term)
    return query
