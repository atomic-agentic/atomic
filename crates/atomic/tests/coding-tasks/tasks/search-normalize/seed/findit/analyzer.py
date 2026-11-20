"""Text to terms: words, folded, without stopwords, each with its position."""

from findit.normalize import fold
from findit.stopwords import is_stopword
from findit.tokenize import words


def terms(text):
    """The terms of text in order. A term's position is its index in this list."""
    out = []
    for word in words(text):
        term = fold(word)
        if term and not is_stopword(term):
            out.append(term)
    return out


def analyze(text):
    """(position, term) pairs for text."""
    return list(enumerate(terms(text)))
