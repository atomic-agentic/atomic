"""Answering a query against an index."""

from findit import phrase, rank
from findit.query import parse


def search(index, text, limit=None):
    """(doc_id, score) for every document matching the query text, best first."""
    query = parse(text)
    if query.is_empty():
        return []
    candidates = set(index.doc_ids)
    for term in query.terms:
        candidates &= index.docs_with(term)
    for p in query.phrases:
        candidates = {doc for doc in candidates if phrase.matches(index, doc, p)}
    for term in query.excluded:
        candidates -= index.docs_with(term)
    looked_for = query.looked_for()
    results = rank.order((doc, rank.score(index, looked_for, doc)) for doc in candidates)
    return results[:limit] if limit is not None else results
