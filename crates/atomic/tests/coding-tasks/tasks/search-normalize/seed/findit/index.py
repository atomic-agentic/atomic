"""The inverted index: for each term, the documents holding it and its positions there."""

from collections import defaultdict

from findit.analyzer import analyze


class Index:
    def __init__(self):
        self._postings = defaultdict(dict)
        self._lengths = {}

    def add(self, doc_id, text):
        if doc_id in self._lengths:
            raise ValueError(f"document {doc_id!r} is already in the index")
        pairs = analyze(text)
        self._lengths[doc_id] = len(pairs)
        for position, term in pairs:
            self._postings[term].setdefault(doc_id, []).append(position)

    @property
    def doc_ids(self):
        return sorted(self._lengths)

    def __len__(self):
        return len(self._lengths)

    def length(self, doc_id):
        return self._lengths[doc_id]

    def docs_with(self, term):
        """The ids of the documents holding term."""
        return set(self._postings.get(term, {}))

    def doc_freq(self, term):
        return len(self._postings.get(term, {}))

    def positions(self, term, doc_id):
        return self._postings.get(term, {}).get(doc_id, [])

    def term_freq(self, term, doc_id):
        return len(self.positions(term, doc_id))
