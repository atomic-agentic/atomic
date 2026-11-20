"""Scores: term frequency, weighted by how rare the term is."""

import math


def term_score(index, term, doc_id):
    length = index.length(doc_id)
    df = index.doc_freq(term)
    if length == 0 or df == 0:
        return 0.0
    tf = index.term_freq(term, doc_id) / length
    return tf * math.log(1 + len(index) / df)


def score(index, terms, doc_id):
    return sum(term_score(index, term, doc_id) for term in terms)


def order(scored):
    """(doc_id, score) pairs sorted best first, then by id; scores compared to nine decimals."""
    return sorted(scored, key=lambda pair: (-round(pair[1], 9), pair[0]))
